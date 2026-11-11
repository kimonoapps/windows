//! Kimono Drive's API, in memory, as far as the desktop app uses it: spaces, folders, the change
//! feed, ranged downloads and tus uploads. Good enough to prove the app against, on any OS.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::client::Client;

pub const TOKEN: &str = "test-token";
pub const SPACE: &str = "space1";
pub const ROOT: &str = "root1";

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub parent: Option<String>,
    pub kind: &'static str,
    pub name: String,
    pub bytes: Vec<u8>,
    pub version: i64,
    pub content_version: i64,
    pub updated: i64,
    pub trashed: bool,
}

struct Upload {
    folder: Option<String>,
    replaces: Option<String>,
    base: i64,
    name: String,
    size: usize,
    received: Vec<u8>,
}

#[derive(Default)]
struct Drive {
    items: HashMap<String, Item>,
    uploads: HashMap<String, Upload>,
    log: Vec<(i64, String)>,
    seq: i64,
    ids: u64,
    space_name: String,
}

#[derive(Clone)]
pub struct Fake {
    drive: Arc<Mutex<Drive>>,
    pub address: String,
    /// Drops the connection partway through every Nth upload piece, to prove uploads resume.
    pub drop_every: Arc<AtomicUsize>,
    pieces: Arc<AtomicUsize>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

impl Drive {
    fn touch(&mut self, id: &str) {
        self.seq += 1;
        let seq = self.seq;
        self.log.push((seq, id.into()));
    }
    fn next_id(&mut self, prefix: &str) -> String {
        self.ids += 1;
        format!("{prefix}{}", self.ids)
    }
    fn json(&self, i: &Item) -> Value {
        json!({ "id": i.id, "spaceId": SPACE, "parentId": i.parent, "kind": i.kind, "name": i.name, "size": i.bytes.len(), "mime": null,
            "createdAt": i.updated, "updatedAt": i.updated, "version": i.version, "contentVersion": i.content_version, "role": "owner" })
    }
    fn space(&self) -> Value {
        json!({ "id": SPACE, "name": self.space_name, "rootId": ROOT, "role": "owner" })
    }
    fn taken(&self, parent: &str, name: &str, except: Option<&str>) -> bool {
        self.items.values().any(|i| i.parent.as_deref() == Some(parent) && !i.trashed && i.name.eq_ignore_ascii_case(name) && Some(i.id.as_str()) != except)
    }
    fn add(&mut self, parent: &str, name: &str, kind: &'static str, bytes: Vec<u8>) -> Item {
        let id = self.next_id("n");
        let item = Item { id: id.clone(), parent: Some(parent.into()), kind, name: name.into(), bytes, version: 1, content_version: 1, updated: now(), trashed: false };
        self.items.insert(id.clone(), item.clone());
        self.touch(&id);
        item
    }
}

impl Fake {
    pub async fn start() -> Self {
        let mut drive = Drive { space_name: "Family".into(), ..Default::default() };
        drive.items.insert(ROOT.into(), Item { id: ROOT.into(), parent: None, kind: "folder", name: "Family".into(), bytes: vec![], version: 1, content_version: 1, updated: now(), trashed: false });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut fake = Fake {
            drive: Arc::new(Mutex::new(drive)),
            address: String::new(),
            drop_every: Default::default(),
            pieces: Default::default(),
            calls: Default::default(),
        };
        fake.address = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/v1/me", get(|| async { Json(json!({ "username": "tester" })) }))
            .route("/api/v1/drive/spaces", get(spaces))
            .route("/api/v1/drive/shared-with-you", get(|| async { Json(json!({ "items": [] })) }))
            .route("/api/v1/drive/nodes/{id}", get(node).patch(update).delete(trash))
            .route("/api/v1/drive/nodes/{id}/folders", post(create_folder))
            .route("/api/v1/drive/nodes/{id}/content", get(content))
            .route("/api/v1/drive/changes", get(changes))
            .route("/api/v1/drive/uploads", post(start_upload))
            .route("/api/v1/drive/uploads/{id}", axum::routing::head(upload_offset).patch(upload_piece))
            .layer(middleware::from_fn_with_state(fake.clone(), auth))
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    pub fn client(&self) -> Client {
        Client::with_token(&self.address, TOKEN)
    }

    pub fn add_folder(&self, parent: &str, name: &str) -> Item { self.drive.lock().add(parent, name, "folder", vec![]) }
    pub fn add_file(&self, parent: &str, name: &str, bytes: &[u8]) -> Item { self.drive.lock().add(parent, name, "file", bytes.to_vec()) }

    pub fn rename(&self, id: &str, name: &str) {
        let mut d = self.drive.lock();
        let i = d.items.get_mut(id).unwrap();
        i.name = name.into();
        i.version += 1;
        d.touch(id);
    }

    pub fn edit(&self, id: &str, bytes: &[u8]) {
        let mut d = self.drive.lock();
        let i = d.items.get_mut(id).unwrap();
        i.bytes = bytes.to_vec();
        i.version += 1;
        i.content_version += 1;
        i.updated = now();
        d.touch(id);
    }

    pub fn remove(&self, id: &str) {
        let mut d = self.drive.lock();
        d.items.get_mut(id).unwrap().trashed = true;
        d.touch(id);
    }

    /// The item at a path under the space's top folder, like "Photos/2024/a.jpg".
    pub fn find(&self, path: &str) -> Option<Item> {
        let d = self.drive.lock();
        let mut at = d.items.get(ROOT)?.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            at = d.items.values().find(|i| i.parent.as_deref() == Some(at.id.as_str()) && !i.trashed && i.name.eq_ignore_ascii_case(part))?.clone();
        }
        Some(at)
    }

    pub fn count(&self, kind: &str) -> usize {
        self.drive.lock().items.values().filter(|i| i.kind == kind && !i.trashed && i.id != ROOT).count()
    }
}

async fn auth(State(fake): State<Fake>, request: Request, next: Next) -> Response {
    fake.calls.lock().push(format!("{} {}", request.method(), request.uri().path()));
    let ok = request.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"));
    if !ok { return StatusCode::UNAUTHORIZED.into_response(); }
    next.run(request).await
}

async fn spaces(State(f): State<Fake>) -> Json<Value> {
    let d = f.drive.lock();
    Json(json!({ "spaces": [d.space()] }))
}

async fn node(State(f): State<Fake>, Path(id): Path<String>) -> Response {
    let d = f.drive.lock();
    let Some(item) = d.items.get(&id).filter(|i| !i.trashed) else { return problem(StatusCode::NOT_FOUND, "That isn't there anymore.") };
    if item.kind == "file" { return Json(json!({ "file": d.json(item) })).into_response(); }
    let mut children: Vec<&Item> = d.items.values().filter(|i| i.parent.as_deref() == Some(id.as_str()) && !i.trashed).collect();
    children.sort_by_key(|i| (i.kind == "file", i.name.to_lowercase()));
    Json(json!({ "folder": d.json(item), "path": [], "children": children.iter().map(|c| d.json(c)).collect::<Vec<_>>() })).into_response()
}

async fn create_folder(State(f): State<Fake>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let name = body["name"].as_str().unwrap_or("").to_string();
    let mut d = f.drive.lock();
    if !d.items.get(&id).is_some_and(|p| p.kind == "folder" && !p.trashed) { return problem(StatusCode::NOT_FOUND, "That isn't there anymore."); }
    if d.taken(&id, &name, None) { return problem(StatusCode::CONFLICT, "Something here already has that name."); }
    let made = d.add(&id, &name, "folder", vec![]);
    (StatusCode::CREATED, Json(d.json(&made))).into_response()
}

async fn update(State(f): State<Fake>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut d = f.drive.lock();
    let Some(item) = d.items.get(&id).filter(|i| !i.trashed).cloned() else { return problem(StatusCode::NOT_FOUND, "That isn't there anymore.") };
    let Some(old_parent) = item.parent.clone() else { return problem(StatusCode::BAD_REQUEST, "Rename the space itself instead.") };
    let parent = body["parent"].as_str().map(String::from).unwrap_or(old_parent);
    let name = body["name"].as_str().map(String::from).unwrap_or(item.name.clone());
    if d.taken(&parent, &name, Some(&id)) { return problem(StatusCode::CONFLICT, "Something here already has that name."); }
    let i = d.items.get_mut(&id).unwrap();
    i.parent = Some(parent);
    i.name = name;
    i.version += 1;
    d.touch(&id);
    let i = d.items[&id].clone();
    Json(d.json(&i)).into_response()
}

async fn trash(State(f): State<Fake>, Path(id): Path<String>) -> Response {
    let mut d = f.drive.lock();
    let Some(item) = d.items.get(&id).filter(|i| !i.trashed).cloned() else { return problem(StatusCode::NOT_FOUND, "That isn't there anymore.") };
    if item.parent.is_none() { return problem(StatusCode::BAD_REQUEST, "A space's top folder can't go in the trash."); }
    let mut stack = vec![id];
    while let Some(next) = stack.pop() {
        d.items.get_mut(&next).unwrap().trashed = true;
        d.touch(&next);
        stack.extend(d.items.values().filter(|c| c.parent.as_deref() == Some(next.as_str()) && !c.trashed).map(|c| c.id.clone()));
    }
    Json(json!({ "trashed": true })).into_response()
}

async fn content(State(f): State<Fake>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let bytes = {
        let d = f.drive.lock();
        match d.items.get(&id).filter(|i| !i.trashed && i.kind == "file") { Some(i) => i.bytes.clone(), None => return StatusCode::NOT_FOUND.into_response() }
    };
    let len = bytes.len();
    if let Some(range) = headers.get("range").and_then(|r| r.to_str().ok()).and_then(|r| r.strip_prefix("bytes=")) {
        let (a, b) = range.split_once('-').unwrap();
        let from: usize = a.parse().unwrap();
        let to = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<usize>().unwrap().min(len.saturating_sub(1)) };
        let part = if from <= to && from < len { bytes[from..=to].to_vec() } else { vec![] };
        return Response::builder().status(206).header("Content-Range", format!("bytes {from}-{to}/{len}")).header("Content-Length", part.len()).body(Body::from(part)).unwrap();
    }
    Response::builder().header("Content-Length", len).body(Body::from(bytes)).unwrap()
}

async fn changes(State(f): State<Fake>, Query(q): Query<HashMap<String, String>>) -> Response {
    let d = f.drive.lock();
    let Some(since) = q.get("since") else { return Json(json!({ "cursor": d.seq })).into_response() };
    let since: i64 = since.parse().unwrap_or(-1);
    if since > d.seq || since < 0 { return problem(StatusCode::GONE, "That cursor isn't from this Drive. List everything again."); }
    let mut touched: Vec<String> = Vec::new();
    for (seq, id) in &d.log { if *seq > since && !touched.contains(id) { touched.push(id.clone()); } }
    let changed: Vec<Value> = touched.iter().filter_map(|id| d.items.get(id)).filter(|i| !i.trashed).map(|i| d.json(i)).collect();
    let removed: Vec<&String> = touched.iter().filter(|id| d.items[*id].trashed).collect();
    Json(json!({ "cursor": d.seq, "changed": changed, "removed": removed, "more": false, "spaces": [d.space()] })).into_response()
}

async fn start_upload(State(f): State<Fake>, headers: HeaderMap) -> Response {
    use base64::Engine;
    let mut meta = HashMap::new();
    for pair in headers.get("upload-metadata").and_then(|v| v.to_str().ok()).unwrap_or("").split(',') {
        let mut kv = pair.trim().splitn(2, ' ');
        let Some(k) = kv.next().filter(|k| !k.is_empty()) else { continue };
        let v = kv.next().map(|v| String::from_utf8(base64::engine::general_purpose::STANDARD.decode(v).unwrap()).unwrap()).unwrap_or_default();
        meta.insert(k.to_string(), v);
    }
    let size: usize = headers.get("upload-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut d = f.drive.lock();
    let (folder, replaces) = (meta.get("folder").cloned(), meta.get("replaces").cloned());
    if folder.is_none() && replaces.is_none() { return problem(StatusCode::BAD_REQUEST, "Say which folder the file goes in."); }
    let name = match &replaces { Some(r) => d.items[r].name.clone(), None => meta.get("filename").cloned().unwrap_or_default() };
    let id = d.next_id("u");
    d.uploads.insert(id.clone(), Upload { folder, replaces, base: meta.get("base").and_then(|b| b.parse().ok()).unwrap_or(0), name, size, received: vec![] });
    Response::builder().status(201).header("Location", format!("/api/v1/drive/uploads/{id}")).header("Upload-Offset", "0").header("Tus-Resumable", "1.0.0").body(Body::empty()).unwrap()
}

async fn upload_offset(State(f): State<Fake>, Path(id): Path<String>) -> Response {
    let d = f.drive.lock();
    match d.uploads.get(&id) {
        Some(u) => Response::builder().header("Upload-Offset", u.received.len()).header("Upload-Length", u.size).body(Body::empty()).unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn upload_piece(State(f): State<Fake>, Path(id): Path<String>, method: Method, headers: HeaderMap, body: Body) -> Response {
    let _ = method;
    let offset: usize = headers.get("upload-offset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    {
        let d = f.drive.lock();
        let Some(u) = d.uploads.get(&id) else { return StatusCode::NOT_FOUND.into_response() };
        if offset != u.received.len() {
            return Response::builder().status(409).header("Upload-Offset", u.received.len()).body(Body::empty()).unwrap();
        }
    }
    let every = f.drop_every.load(Ordering::Relaxed);
    let drop = every > 0 && (f.pieces.fetch_add(1, Ordering::Relaxed) + 1) % every == 0;
    let mut stream = body.into_data_stream();
    let mut taken = 0usize;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk): Result<Bytes, _> = chunk else { break };
        taken += chunk.len();
        f.drive.lock().uploads.get_mut(&id).unwrap().received.extend_from_slice(&chunk);
        if drop && taken >= 1024 {
            /* Gone mid-piece, as a dropped connection looks from the server. */
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let mut d = f.drive.lock();
    let u = d.uploads.get(&id).unwrap();
    let mut response = Response::builder().status(204).header("Upload-Offset", u.received.len());
    if u.received.len() == u.size {
        let u = d.uploads.remove(&id).unwrap();
        let made_id = match &u.replaces {
            Some(target) if d.items[target].content_version != u.base => {
                let t = d.items[target].clone();
                let (stem, ext) = match t.name.rsplit_once('.') { Some((s, e)) => (s.to_string(), format!(".{e}")), None => (t.name.clone(), String::new()) };
                let made = d.add(t.parent.as_deref().unwrap(), &format!("{stem} (edited elsewhere){ext}"), "file", u.received);
                response = response.header("Kimono-Conflict", "kept-as-copy");
                made.id
            }
            Some(target) => {
                let target = target.clone();
                let i = d.items.get_mut(&target).unwrap();
                i.bytes = u.received;
                i.content_version += 1;
                i.version += 1;
                i.updated = now();
                d.touch(&target);
                target
            }
            None => {
                let folder = u.folder.clone().unwrap();
                let mut name = u.name.clone();
                let mut k = 2;
                while d.taken(&folder, &name, None) {
                    name = match u.name.rsplit_once('.') { Some((s, e)) => format!("{s} ({k}).{e}"), None => format!("{} ({k})", u.name) };
                    k += 1;
                }
                d.add(&folder, &name, "file", u.received).id
            }
        };
        response = response.header("Kimono-File", made_id);
    }
    response.body(Body::empty()).unwrap()
}
