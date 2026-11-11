//! Signing in without a browser: the same steps the sign-in page takes (who you are, your
//! password, a code if you use one), answered through the identity provider's flow API, ending
//! at the authorisation code the page would hand back. People only ever see Kimono.
//! A port of the phone app's SignInFlow.

use std::sync::{Arc, LazyLock};

use regex::Regex;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderValue, LOCATION};
use serde_json::{json, Map, Value};
use url::Url;

use crate::models::SignInNext;
use crate::problem::{Problem, Result};

const FAILED: &str = "Signing in didn't finish. Try again.";
static FLOW: LazyLock<Regex> = LazyLock::new(|| Regex::new("/if/flow/([^/]+)/").unwrap());

pub(crate) struct Flow {
    redirect: String,
    issuer: Url,
    jar: Arc<Jar>,
    http: reqwest::Client,
    executor: Option<Url>,
    origin: Url,
    challenge: Option<Map<String, Value>>,
    pub code: Option<String>,
    pub returned_state: Option<String>,
}

fn fail() -> Problem {
    Problem::say(FAILED)
}

impl Flow {
    pub fn new(redirect: &str, issuer: &str, user_agent: &str) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        /* Its own jar, and redirects taken one at a time so the app's own address can be caught. */
        let http = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            redirect: redirect.into(),
            issuer: Url::parse(issuer).map_err(|_| fail())?,
            jar,
            http,
            executor: None,
            origin: Url::parse("http://localhost").unwrap(),
            challenge: None,
            code: None,
            returned_state: None,
        })
    }

    fn resolve(&self, from: &Url, location: &str) -> Result<Url> {
        if location.starts_with(&self.redirect) {
            return Url::parse(location).map_err(|_| fail());
        }
        from.join(location).map_err(|_| fail())
    }

    /// Opens the authorisation address and follows it to the sign-in flow.
    pub async fn start(&mut self, authorize: &str) -> Result<()> {
        let mut url = Url::parse(authorize).map_err(|_| fail())?;
        for _ in 0..10 {
            if self.caught(&url)? { return Ok(()); }
            let response = self.http.get(url.clone()).send().await?;
            let location = location(&response).ok_or_else(fail)?;
            if !response.status().is_redirection() { return Err(fail()); }
            url = self.resolve(&url, &location)?;
            if self.caught(&url)? { return Ok(()); }
            if let Some(slug) = FLOW.captures(url.path()).map(|c| c[1].to_string()) {
                if !url.host_str().unwrap_or("").eq_ignore_ascii_case(self.issuer.host_str().unwrap_or("")) || url.scheme() != self.issuer.scheme() {
                    return Err(Problem::say("Signing in led somewhere other than Kimono."));
                }
                /* The origin as the server writes it: no ":443" on https (Authentik's CSRF check compares it exactly). */
                self.origin = Url::parse(&url.origin().ascii_serialization()).map_err(|_| fail())?;
                return self.enter(&slug, &url).await;
            }
        }
        Err(fail())
    }

    async fn enter(&mut self, slug: &str, page: &Url) -> Result<()> {
        let query = page.query().unwrap_or("");
        let mut executor = self.origin.join(&format!("/api/v3/flows/executor/{slug}/")).map_err(|_| fail())?;
        executor.query_pairs_mut().append_pair("query", query);
        self.executor = Some(executor.clone());
        let response = self.http.get(executor).send().await?;
        self.challenge = Some(Box::pin(self.read(response)).await?);
        Ok(())
    }

    /// Answers the flow's questions with what the person typed, until it needs more or is done.
    pub async fn answer(&mut self, username: &str, password: &str, otp: Option<&str>) -> Result<SignInNext> {
        for _ in 0..12 {
            if self.code.is_some() { return Ok(SignInNext::Done); }
            let current = self.challenge.clone().ok_or_else(fail)?;
            let component = text(&current, "component").ok_or_else(fail)?;
            if let Some(shown) = problem_in(&current) { return Err(Problem::say(shown)); }
            let reply = match component.as_str() {
                "ak-stage-identification" => {
                    let mut reply = json!({ "component": component, "uid_field": username });
                    if text(&current, "password_fields").as_deref() == Some("true") { reply["password"] = json!(password); }
                    reply
                }
                "ak-stage-password" => json!({ "component": component, "password": password }),
                "ak-stage-authenticator-validate" => {
                    let kinds: Vec<String> = current.get("device_challenges").and_then(Value::as_array).into_iter().flatten()
                        .filter_map(|d| d.get("device_class").and_then(Value::as_str).map(String::from)).collect();
                    if !kinds.iter().any(|k| k == "totp" || k == "static") {
                        return Err(Problem::say("This account signs in with a security key, which this app can't use yet. Sign in on the website."));
                    }
                    let Some(otp) = otp else { return Ok(SignInNext::Code) };
                    json!({ "component": component, "code": otp })
                }
                /* The app is Kimono's own: agree to it on the person's behalf. */
                "ak-stage-consent" => json!({ "component": component, "token": text(&current, "token").unwrap_or_default() }),
                "ak-stage-user-login" => json!({ "component": component, "remember_me": false }),
                "xak-flow-redirect" => {
                    let to = text(&current, "to").ok_or_else(fail)?;
                    self.follow(&to).await?;
                    continue;
                }
                "ak-stage-flow-error" => return Err(Problem::say(text(&current, "error").filter(|e| !e.is_empty()).unwrap_or(FAILED.into()))),
                "ak-stage-access-denied" => return Err(Problem::say(text(&current, "error_message").unwrap_or("This account can't sign in here.".into()))),
                _ => return Err(Problem::say("This account needs a step the app can't do yet. Sign in on the website first.")),
            };
            let response = self.post(reply).await?;
            let next = self.read(response).await?;
            /* A wrong password comes back as the same step with errors. */
            if let Some(message) = problem_in(&next) {
                if component == "ak-stage-authenticator-validate" { return Err(Problem::say("That code didn't work. Try the newest one.")); }
                return Err(Problem::say(message));
            }
            self.challenge = Some(next);
        }
        if self.code.is_some() { Ok(SignInNext::Done) } else { Err(fail()) }
    }

    async fn post(&self, reply: Value) -> Result<reqwest::Response> {
        let executor = self.executor.clone().ok_or_else(fail)?;
        let mut request = self.http.post(executor).json(&reply).header("Referer", self.origin.as_str());
        if let Some(csrf) = self.cookie("authentik_csrf") { request = request.header("X-authentik-CSRF", csrf); }
        Ok(request.send().await?)
    }

    fn cookie(&self, name: &str) -> Option<String> {
        let header: HeaderValue = self.jar.cookies(&self.origin)?;
        header.to_str().ok()?.split(';').map(str::trim).find_map(|pair| pair.strip_prefix(&format!("{name}=")).map(String::from))
    }

    /// Reads the next step, following the flow's own redirects back to itself.
    async fn read(&mut self, first: reqwest::Response) -> Result<Map<String, Value>> {
        let mut response = first;
        for _ in 0..5 {
            if response.status().is_redirection() {
                if let Some(location) = location(&response) {
                    let next = self.resolve(&self.origin.clone(), &location)?;
                    if !next.as_str().contains("/flows/executor/") {
                        Box::pin(self.follow(next.as_str())).await?;
                        return if self.code.is_some() { Ok(Map::from_iter([("component".into(), json!("done"))])) } else { self.challenge.clone().ok_or_else(fail) };
                    }
                    response = self.http.get(next).send().await?;
                    continue;
                }
            }
            let body: Value = response.json().await.map_err(|_| fail())?;
            return body.as_object().cloned().ok_or_else(fail);
        }
        Err(fail())
    }

    /// Follows an address the flow sends us to, catching the app's own address at the end.
    async fn follow(&mut self, to: &str) -> Result<()> {
        let mut url = self.resolve(&self.origin.clone(), to)?;
        for _ in 0..10 {
            if self.caught(&url)? { return Ok(()); }
            let response = self.http.get(url.clone()).send().await?;
            let Some(location) = location(&response) else {
                /* Another flow (consent, say): carry on there. */
                let slug = FLOW.captures(url.path()).map(|c| c[1].to_string()).ok_or_else(fail)?;
                return Box::pin(self.enter(&slug, &url)).await;
            };
            url = self.resolve(&url, &location)?;
            if self.caught(&url)? { return Ok(()); }
            if let Some(slug) = FLOW.captures(url.path()).map(|c| c[1].to_string()) {
                return Box::pin(self.enter(&slug, &url)).await;
            }
        }
        Err(fail())
    }

    fn caught(&mut self, url: &Url) -> Result<bool> {
        if !url.as_str().starts_with(&self.redirect) { return Ok(false); }
        let pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        self.code = Some(get("code").ok_or_else(|| Problem::say(get("error_description").unwrap_or(FAILED.into())))?);
        self.returned_state = get("state");
        Ok(true)
    }
}

fn location(response: &reqwest::Response) -> Option<String> {
    response.headers().get(LOCATION)?.to_str().ok().map(String::from)
}

fn text(o: &Map<String, Value>, key: &str) -> Option<String> {
    match o.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The first error the flow gives for what was sent, in its own words.
fn problem_in(step: &Map<String, Value>) -> Option<String> {
    let errors = step.get("response_errors")?.as_object()?;
    let first = errors.values().find_map(Value::as_array)?;
    let Some(text) = first.first().and_then(|e| e.get("string")).and_then(Value::as_str) else {
        return Some("That didn't work. Try again.".into());
    };
    let lower = text.to_lowercase();
    Some(if lower.contains("password") || lower.contains("invalid") { "That username or password isn't right.".into() } else { text.into() })
}
