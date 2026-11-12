//! Sends a file to Drive over tus, in pieces, picking up where it stopped after a dropped
//! connection, and across runs when a resume store is given.

use std::collections::{BTreeMap, HashMap};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use reqwest::{Body, Method, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::client::Client;
use crate::models::Uploaded;
use crate::problem::{Patience, Problem, Result};

/// Pieces of 32 MB: under the 50 MB some proxies allow, big enough to keep a fast link full.
pub const PIECE: u64 = 32 * 1024 * 1024;

pub type Metadata = BTreeMap<String, String>;

pub fn new_file(folder: &str, name: &str) -> Metadata {
    BTreeMap::from([("filename".into(), name.into()), ("folder".into(), folder.into())])
}

pub fn replace(file: &str, base_content_version: i64) -> Metadata {
    BTreeMap::from([("replaces".into(), file.into()), ("base".into(), base_content_version.to_string())])
}

/// Where unfinished uploads are on the server, so a restart carries on instead of starting over.
pub trait ResumeStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, location: Option<&str>);
}

/// Kept in a small JSON file.
pub struct FileResume {
    path: PathBuf,
    kept: Mutex<HashMap<String, String>>,
}

impl FileResume {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let kept = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, kept: Mutex::new(kept) }
    }
    fn save(&self, kept: &HashMap<String, String>) {
        if let Some(dir) = self.path.parent() { let _ = std::fs::create_dir_all(dir); }
        let temp = self.path.with_extension("tmp");
        if std::fs::write(&temp, serde_json::to_vec(kept).unwrap_or_default()).is_ok() { let _ = std::fs::rename(temp, &self.path); }
    }
}

impl ResumeStore for FileResume {
    fn get(&self, key: &str) -> Option<String> { self.kept.lock().get(key).cloned() }
    fn set(&self, key: &str, location: Option<&str>) {
        let mut kept = self.kept.lock();
        match location { Some(l) => { kept.insert(key.into(), l.into()); } None => { kept.remove(key); } }
        self.save(&kept);
    }
}

/// Bytes sent since the last call; negative when a dropped piece is taken back.
pub type Progress = Arc<dyn Fn(i64) + Send + Sync>;

/// What to upload: a file on disk, or bytes in memory.
#[derive(Clone)]
pub enum Source {
    File(PathBuf),
    Memory(Bytes),
}

impl Source {
    pub fn file(path: impl AsRef<Path>) -> Self { Self::File(path.as_ref().to_path_buf()) }

    /// `length` bytes from `offset`, read as they are sent, counted through `count`.
    /// `sent` counts bytes read for this attempt while it is `Some`; the attempt closes it (to `None`) when it
    /// ends, so a chunk read after the server has already cut the piece off is never counted.
    fn piece(&self, offset: u64, length: u64, progress: Progress, sent: Arc<Mutex<Option<i64>>>) -> Body {
        let count = move |n: usize| {
            let mut open = sent.lock();
            if let Some(total) = open.as_mut() { *total += n as i64; progress(n as i64); }
        };
        match self {
            Self::Memory(bytes) => {
                let piece = bytes.slice(offset as usize..(offset + length) as usize);
                count(piece.len());
                Body::from(piece)
            }
            Self::File(path) => {
                let path = path.clone();
                let opened = futures::stream::once(async move {
                    let mut file = tokio::fs::File::open(&path).await?;
                    file.seek(SeekFrom::Start(offset)).await?;
                    Ok::<_, std::io::Error>(tokio_stream(file.take(length), 1 << 20))
                });
                let stream = opened.try_flatten().map(move |chunk| { if let Ok(c) = &chunk { count(c.len()); } chunk });
                Body::wrap_stream(stream)
            }
        }
    }
}

/// Reads an async reader as a stream of chunks, without pulling in tokio-util.
fn tokio_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(reader: R, chunk: usize) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    futures::stream::unfold((reader, vec![0u8; chunk]), |(mut reader, mut buf)| async move {
        match reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => Some((Ok(Bytes::copy_from_slice(&buf[..n])), (reader, buf))),
            Err(e) => Some((Err(e), (reader, buf))),
        }
    })
}

fn header(response: &Response, name: &str) -> Option<String> {
    response.headers().get(name)?.to_str().ok().map(String::from)
}

/// Uploads `size` bytes from `source` with tus `metadata`.
pub async fn send(client: &Client, source: Source, size: u64, metadata: &Metadata, progress: Option<Progress>, resume: Option<&dyn ResumeStore>) -> Result<Uploaded> {
    let progress: Progress = progress.unwrap_or_else(|| Arc::new(|_| {}));
    /* Which upload on the server this is: what it's called and where, its size, and (for a file) when it was
       last written, so a file rewritten at the same size while half sent starts over instead of mixing. */
    let modified = match &source {
        Source::File(path) => std::fs::metadata(path).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0),
        Source::Memory(_) => 0,
    };
    let key = format!("{}:{size}:{modified}", metadata.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"));
    let mut offset = 0u64;
    let mut location = resume.and_then(|r| r.get(&key));
    if let Some(known) = &location {
        match current_offset(client, known).await {
            Ok(at) => offset = at,
            Err(p) if p.status == 404 => location = None,
            Err(p) => return Err(p),
        }
    }
    let mut location = match location {
        Some(l) => l,
        None => {
            let made = create(client, size, metadata).await?;
            if let Some(r) = resume { r.set(&key, Some(&made)); }
            made
        }
    };
    progress(offset as i64);

    let mut patience = Patience::new();
    loop {
        /* Held here while paused (on a phone's hotspot, say). */
        crate::net::go_ahead().await;
        let length = PIECE.min(size - offset);
        let sent = Arc::new(Mutex::new(Some(0i64)));
        let at = offset;
        let attempt = client.send(|| {
            /* Each try reads the piece afresh from where the server has it. */
            let before = sent.lock().replace(0).unwrap_or(0);
            if before != 0 { progress(-before); }
            let body = source.piece(at, length, progress.clone(), sent.clone());
            Ok(client.request(Method::PATCH, &location)?
                .header("Tus-Resumable", "1.0.0")
                .header("Upload-Offset", at.to_string())
                .header("Content-Type", "application/offset+octet-stream")
                .header("Content-Length", length.to_string())
                .timeout(Duration::from_secs(600))
                .body(body))
        }, true).await;
        let taken = sent.lock().take().unwrap_or(0);
        match attempt {
            Ok(response) if response.status() == StatusCode::CONFLICT => {
                /* Out of step (a dropped piece landed after all): carry on from where the server is. */
                progress(-taken);
                let now = match header(&response, "Upload-Offset").and_then(|o| o.parse().ok()) { Some(o) => o, None => current_offset(client, &location).await? };
                progress(now as i64 - offset as i64);
                offset = now;
            }
            Ok(response) => {
                let landed: u64 = header(&response, "Upload-Offset").and_then(|o| o.parse().ok()).unwrap_or(offset + length);
                let counted = offset as i64 + taken;
                if landed as i64 != counted { progress(landed as i64 - counted); }
                offset = landed;
                patience.reset();
                if let Some(id) = header(&response, "Kimono-File") {
                    if let Some(r) = resume { r.set(&key, None); }
                    return Ok(Uploaded { id, conflict: header(&response, "Kimono-Conflict").is_some() });
                }
                if offset >= size {
                    /* Every byte is there but Kimono didn't name the file: something's off. */
                    return Err(Problem::unreachable());
                }
            }
            Err(p) if p.offline() || p.busy() => {
                progress(-taken);
                if !patience.wait(&p).await { return Err(p); }
                match current_offset(client, &location).await {
                    Ok(now) => { progress(now as i64 - offset as i64); offset = now; }
                    Err(gone) if gone.status == 404 => {
                        /* The server let the unfinished upload go: start it again. */
                        progress(-(offset as i64));
                        offset = 0;
                        location = create(client, size, metadata).await?;
                        if let Some(r) = resume { r.set(&key, Some(&location)); }
                    }
                    Err(_) => {}
                }
            }
            Err(p) => {
                progress(-taken);
                return Err(p);
            }
        }
    }
}

async fn create(client: &Client, size: u64, metadata: &Metadata) -> Result<String> {
    let encoded = metadata.iter().map(|(k, v)| format!("{k} {}", STANDARD.encode(v))).collect::<Vec<_>>().join(",");
    let mut patience = Patience::new();
    loop {
        crate::net::go_ahead().await;
        let attempt = client.send(|| Ok(client.request(Method::POST, "/api/v1/drive/uploads")?
            .header("Tus-Resumable", "1.0.0")
            .header("Upload-Length", size.to_string())
            .header("Upload-Metadata", &encoded)), false).await;
        match attempt {
            Ok(response) => return header(&response, "Location").ok_or_else(|| Problem::say("Kimono didn't take the upload.")),
            /* Busy: Kimono is holding room for this person's other uploads (200 GB at once); offline: wait it out. */
            Err(p) => if !patience.wait(&p).await { return Err(p); },
        }
    }
}

async fn current_offset(client: &Client, location: &str) -> Result<u64> {
    let response = client.send(|| Ok(client.request(Method::HEAD, location)?.header("Tus-Resumable", "1.0.0")), false).await?;
    header(&response, "Upload-Offset").and_then(|o| o.parse().ok()).ok_or_else(|| Problem::new("That upload isn't there anymore.", 404))
}
