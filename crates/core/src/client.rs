//! Signing in, staying signed in, and talking to the Portal, as the phone app does: the Portal
//! says where to sign in, the steps run in-app with PKCE, only the refresh token is kept (by the
//! system keychain), and access tokens live in memory. Cheap to clone; safe across tasks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rand::RngCore;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::models::*;
use crate::problem::{Problem, Result};
use crate::secrets::SecretStore;
use crate::signin::Flow;

pub const REDIRECT: &str = "kimono://auth";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn user_agent() -> String {
    format!("Kimono-Desktop/{VERSION} ({})", std::env::consts::OS)
}

#[derive(Clone)]
pub struct Client(Arc<Inner>);

struct Inner {
    http: reqwest::Client,
    store: Arc<dyn SecretStore>,
    fixed_token: Option<String>,
    access: Mutex<Option<(String, Instant)>>,
    token_endpoint: Mutex<Option<String>>,
    renewing: tokio::sync::Mutex<()>,
    pending: tokio::sync::Mutex<Option<Pending>>,
    signed_out: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

struct Pending {
    flow: Flow,
    server: String,
    config: SignInConfig,
    token_endpoint: String,
    verifier: String,
    state: String,
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

pub fn normalize_address(address: &str) -> String {
    let trimmed = address.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") { trimmed.into() } else { format!("https://{trimmed}") }
}

fn random(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Client {
    /// Many uploads and downloads at once, over as few connections as HTTP/2 allows.
    fn http() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent(user_agent())
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(64)
            .connect_timeout(Duration::from_secs(20))
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_while_idle(true)
            .build()
            .expect("an HTTP client")
    }

    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self(Arc::new(Inner {
            http: Self::http(),
            store,
            fixed_token: None,
            access: Mutex::new(None),
            token_endpoint: Mutex::new(None),
            renewing: tokio::sync::Mutex::new(()),
            pending: tokio::sync::Mutex::new(None),
            signed_out: Mutex::new(None),
        }))
    }

    /// A client with a token handed to it, for tests and tools: no sign-in.
    pub fn with_token(server: &str, token: &str) -> Self {
        let store = Arc::new(crate::MemoryStore::default());
        store.set("kimono.server", Some(server.trim_end_matches('/')));
        let mut inner = Arc::try_unwrap(Self::new(store).0).ok().unwrap();
        inner.fixed_token = Some(token.into());
        Self(Arc::new(inner))
    }

    /// Called when Kimono refuses the kept sign-in (signed out elsewhere, account switched off).
    pub fn on_signed_out(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.0.signed_out.lock() = Some(Box::new(f));
    }

    pub fn server(&self) -> Option<String> {
        self.0.store.get("kimono.server")
    }

    pub fn is_signed_in(&self) -> bool {
        self.0.fixed_token.is_some() || self.0.store.get("kimono.refresh").is_some()
    }

    // ---- Signing in ----

    async fn discover(&self, issuer: &str) -> Result<Discovery> {
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let response = self.0.http.get(url).send().await?;
        if !response.status().is_success() { return Err(Problem::unreachable()); }
        response.json().await.map_err(|_| Problem::unreachable())
    }

    /* An answer that isn't the provider's (a gateway's error page, a 5xx) reads as offline, never as a refusal. */
    async fn tokens(&self, endpoint: &str, form: &[(&str, &str)]) -> Result<TokenAnswer> {
        let response = self.0.http.post(endpoint).form(form).send().await?;
        if response.status().is_server_error() { return Err(Problem::unreachable()); }
        response.json().await.map_err(|_| Problem::unreachable())
    }

    /// Signs in with a username and password; an account with an authenticator app asks for its code next.
    pub async fn sign_in(&self, address: &str, username: &str, password: &str) -> Result<SignInNext> {
        if address.trim().is_empty() { return Err(Problem::say("Enter your Kimono's address.")); }
        if username.trim().is_empty() { return Err(Problem::say("Enter your username.")); }
        if password.is_empty() { return Err(Problem::say("Enter your password.")); }
        let server = normalize_address(address);
        let nobody = || Problem::say("There's no Kimono at that address.");
        let response = self.0.http.get(format!("{server}/api/v1/sign-in")).send().await.map_err(|_| nobody())?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(Problem::new(failure_of(response).await.unwrap_or_else(|| nobody().message), status));
        }
        let config: SignInConfig = response.json().await.map_err(|_| nobody())?;
        let discovery = self.discover(&config.issuer).await?;

        let verifier = random(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random(16);
        let mut url = url::Url::parse(&discovery.authorization_endpoint).map_err(|_| Problem::unreachable())?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", REDIRECT)
            .append_pair("scope", &config.scopes.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);

        let mut flow = Flow::new(REDIRECT, &config.issuer, &user_agent())?;
        flow.start(url.as_str()).await?;
        let next = flow.answer(username.trim(), password, None).await?;
        let waiting = Pending { flow, server, config, token_endpoint: discovery.token_endpoint, verifier, state, username: username.trim().into(), password: password.into() };
        if next == SignInNext::Code {
            *self.0.pending.lock().await = Some(waiting);
            return Ok(next);
        }
        self.finish(waiting).await?;
        Ok(SignInNext::Done)
    }

    /// The code from the authenticator app, for a sign-in that asked for one.
    pub async fn sign_in_code(&self, code: &str) -> Result<SignInNext> {
        let mut slot = self.0.pending.lock().await;
        let waiting = slot.as_mut().ok_or_else(|| Problem::say("Signing in didn't finish. Try again."))?;
        let code: String = code.chars().filter(|c| c.is_alphanumeric()).collect();
        let (username, password) = (waiting.username.clone(), waiting.password.clone());
        if waiting.flow.answer(&username, &password, Some(&code)).await? == SignInNext::Code { return Ok(SignInNext::Code); }
        let waiting = slot.take().unwrap();
        drop(slot);
        self.finish(waiting).await?;
        Ok(SignInNext::Done)
    }

    pub async fn cancel_sign_in(&self) {
        *self.0.pending.lock().await = None;
    }

    async fn finish(&self, waiting: Pending) -> Result<()> {
        let failed = || Problem::say("Signing in didn't finish. Try again.");
        let code = waiting.flow.code.clone().ok_or_else(failed)?;
        if waiting.flow.returned_state.as_deref() != Some(waiting.state.as_str()) { return Err(failed()); }
        let got = self.tokens(&waiting.token_endpoint, &[
            ("grant_type", "authorization_code"), ("client_id", &waiting.config.client_id), ("code", &code),
            ("redirect_uri", REDIRECT), ("code_verifier", &waiting.verifier),
        ]).await.map_err(|_| failed())?;
        let (Some(access), Some(refresh)) = (got.access_token, got.refresh_token) else { return Err(failed()) };
        let store = &self.0.store;
        store.set("kimono.server", Some(&waiting.server));
        store.set("kimono.issuer", Some(&waiting.config.issuer));
        store.set("kimono.clientId", Some(&waiting.config.client_id));
        store.set("kimono.refresh", Some(&refresh));
        *self.0.token_endpoint.lock() = Some(waiting.token_endpoint);
        *self.0.access.lock() = Some((access, Instant::now() + Duration::from_secs(got.expires_in.unwrap_or(300))));
        Ok(())
    }

    /// Forgets the sign-in here, and revokes it when Kimono can be reached.
    pub async fn sign_out(&self) {
        let store = &self.0.store;
        let (refresh, issuer, client) = (store.get("kimono.refresh"), store.get("kimono.issuer"), store.get("kimono.clientId"));
        store.set("kimono.refresh", None);
        *self.0.access.lock() = None;
        let (Some(refresh), Some(issuer), Some(client)) = (refresh, issuer, client) else { return };
        let revoke = async {
            let endpoint = self.discover(&issuer).await.ok()?.revocation_endpoint?;
            self.0.http.post(endpoint).form(&[("token", refresh.as_str()), ("client_id", client.as_str()), ("token_type_hint", "refresh_token")]).send().await.ok()
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), revoke).await;
    }

    async fn token(&self) -> Result<String> {
        if let Some(t) = &self.0.fixed_token { return Ok(t.clone()); }
        let fresh = |a: &Option<(String, Instant)>| a.as_ref().filter(|(_, until)| *until > Instant::now() + Duration::from_secs(60)).map(|(t, _)| t.clone());
        if let Some(t) = fresh(&self.0.access.lock()) { return Ok(t); }
        let _turn = self.0.renewing.lock().await;
        if let Some(t) = fresh(&self.0.access.lock()) { return Ok(t); }
        let store = &self.0.store;
        let (Some(issuer), Some(client), Some(refresh)) = (store.get("kimono.issuer"), store.get("kimono.clientId"), store.get("kimono.refresh")) else {
            return Err(Problem::new("Sign in to Kimono first.", 401));
        };
        let known = self.0.token_endpoint.lock().clone();
        let endpoint = match known { Some(e) => e, None => self.discover(&issuer).await?.token_endpoint };
        *self.0.token_endpoint.lock() = Some(endpoint.clone());
        let got = self.tokens(&endpoint, &[("grant_type", "refresh_token"), ("client_id", &client), ("refresh_token", &refresh)]).await?;
        let Some(access) = got.access_token else {
            /* Refused: this sign-in is over. */
            store.set("kimono.refresh", None);
            *self.0.access.lock() = None;
            if let Some(f) = &*self.0.signed_out.lock() { f(); }
            return Err(Problem::new("Sign in to Kimono again.", 401));
        };
        if let Some(renewed) = got.refresh_token.filter(|r| *r != refresh) { store.set("kimono.refresh", Some(&renewed)); }
        *self.0.access.lock() = Some((access.clone(), Instant::now() + Duration::from_secs(got.expires_in.unwrap_or(300))));
        Ok(access)
    }

    // ---- Calls ----

    /// A full address for a Portal path (or the path itself when it is already a full address).
    pub fn url(&self, path: &str) -> Result<String> {
        if path.starts_with("http://") || path.starts_with("https://") { return Ok(path.into()); }
        let server = self.server().ok_or_else(|| Problem::new("Sign in to Kimono first.", 401))?;
        Ok(format!("{}{}", server.trim_end_matches('/'), path))
    }

    pub fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        Ok(self.0.http.request(method, self.url(path)?))
    }

    /// Sends a request with the sign-in attached, turning refusals into problems. `build` makes
    /// the request afresh for a second try after an expired token. 409 passes through when asked.
    pub async fn send(&self, build: impl Fn() -> Result<RequestBuilder>, allow_conflict: bool) -> Result<Response> {
        for attempt in 0..2 {
            let response = build()?.bearer_auth(self.token().await?).send().await?;
            let status = response.status();
            if status.is_success() || (allow_conflict && status == StatusCode::CONFLICT) { return Ok(response); }
            if status == StatusCode::UNAUTHORIZED && attempt == 0 && self.0.fixed_token.is_none() {
                /* The token may have just been retired: renew once and ask again. */
                *self.0.access.lock() = None;
                continue;
            }
            let code = status.as_u16();
            let fallback = match code {
                401 => "Sign in to Kimono again.",
                403 => "You can only view this.",
                404 => "That isn't there anymore.",
                507 => "Kimono's disk is full.",
                500.. => "Kimono isn't answering. Try again in a moment.",
                _ => "Kimono couldn't do that.",
            };
            let message = failure_of(response).await.unwrap_or_else(|| fallback.into());
            return Err(Problem::new(message, if code >= 500 { 0 } else { code }));
        }
        unreachable!()
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.send(|| self.request(Method::GET, path), false).await?;
        response.json().await.map_err(|_| Problem::unreachable())
    }

    async fn send_json<T: DeserializeOwned>(&self, method: Method, path: &str, body: Value) -> Result<T> {
        let response = self.send(|| Ok(self.request(method.clone(), path)?.json(&body)), false).await?;
        response.json().await.map_err(|_| Problem::unreachable())
    }

    pub async fn me(&self) -> Result<Me> {
        self.get("/api/v1/me").await
    }

    pub async fn spaces(&self) -> Result<Vec<Space>> {
        #[derive(Deserialize)]
        struct List { spaces: Vec<Space> }
        Ok(self.get::<List>("/api/v1/drive/spaces").await?.spaces)
    }

    pub async fn folder(&self, id: &str) -> Result<FolderListing> {
        self.get(&format!("/api/v1/drive/nodes/{}", enc(id))).await
    }

    pub async fn shared_with_you(&self) -> Result<Vec<Node>> {
        #[derive(Deserialize)]
        struct List { items: Vec<Node> }
        Ok(self.get::<List>("/api/v1/drive/shared-with-you").await?.items)
    }

    pub async fn node(&self, id: &str) -> Result<Node> {
        let body: Value = self.get(&format!("/api/v1/drive/nodes/{}", enc(id))).await?;
        let node = body.get("file").or_else(|| body.get("folder")).cloned().ok_or_else(Problem::unreachable)?;
        serde_json::from_value(node).map_err(|_| Problem::unreachable())
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        let body: Value = self.get("/api/v1/drive/changes").await?;
        body["cursor"].as_i64().ok_or_else(Problem::unreachable)
    }

    /// What changed since a cursor. A 410 means list everything again.
    pub async fn changes(&self, since: i64) -> Result<Changes> {
        self.get(&format!("/api/v1/drive/changes?since={since}")).await
    }

    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<Node> {
        self.send_json(Method::POST, &format!("/api/v1/drive/nodes/{}/folders", enc(parent)), json!({ "name": name })).await
    }

    /// The folder at `path` (names, top down) under `root`, making any that aren't there. Returns its id.
    pub async fn ensure_path(&self, root: &str, path: &[String]) -> Result<String> {
        let mut at = root.to_string();
        for name in path {
            let listing = self.folder(&at).await?;
            at = match listing.children.into_iter().find(|c| c.is_folder() && c.name.eq_ignore_ascii_case(name)) {
                Some(found) => found.id,
                None => match self.create_folder(&at, name).await {
                    Ok(made) => made.id,
                    /* Made meanwhile by another run: look again. */
                    Err(p) if p.status == 409 => self.folder(&at).await?.children.into_iter()
                        .find(|c| c.is_folder() && c.name.eq_ignore_ascii_case(name)).map(|c| c.id).ok_or(p)?,
                    Err(p) => return Err(p),
                },
            };
        }
        Ok(at)
    }

    /// Renames and/or moves.
    pub async fn update(&self, id: &str, name: Option<&str>, parent: Option<&str>) -> Result<Node> {
        let mut body = json!({});
        if let Some(n) = name { body["name"] = json!(n); }
        if let Some(p) = parent { body["parent"] = json!(p); }
        self.send_json(Method::PATCH, &format!("/api/v1/drive/nodes/{}", enc(id)), body).await
    }

    /// Moves to the trash, where it stays for 30 days.
    pub async fn trash(&self, id: &str) -> Result<()> {
        self.send(|| self.request(Method::DELETE, &format!("/api/v1/drive/nodes/{}", enc(id))), false).await?;
        Ok(())
    }

    /// A file's bytes, from `from` up to and including `to` when given, as a stream.
    pub async fn content(&self, id: &str, range: Option<(u64, Option<u64>)>) -> Result<Response> {
        let path = format!("/api/v1/drive/nodes/{}/content", enc(id));
        self.send(|| {
            let mut request = self.request(Method::GET, &path)?;
            if let Some((from, to)) = range {
                request = request.header("Range", match to { Some(to) => format!("bytes={from}-{to}"), None => format!("bytes={from}-") });
            }
            Ok(request)
        }, false).await
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn failure_of(response: Response) -> Option<String> {
    let body: Value = response.json().await.ok()?;
    body.get("error")?.as_str().filter(|s| !s.is_empty()).map(String::from)
}
