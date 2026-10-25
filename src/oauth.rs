/// GitHub and Google sign-in for the web editors.
///
/// Ported from phext-lanes. The session is an HttpOnly cookie signed with
/// `PHEXT_SESSION_SECRET`. The authorization-code flow uses PKCE and a
/// `state` value kept in a second short-lived cookie. Writes (any POST)
/// also need the session's CSRF token in `x-csrf-token`.
///
/// Signing in is not enough to edit: the account must be listed in
/// `PHEXT_EDITORS` (comma-separated `github:<id>`, `google:<sub>`, or email).
/// The `--token` bearer token keeps working alongside sign-in for scripts.
///
/// `Gate` is the piece the HTTP servers use: `route()` answers the
/// `/api/session` and `/api/auth/*` endpoints, `admit()` decides whether
/// any other request may proceed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const SESSION_COOKIE: &str = "phext_session";
const PENDING_COOKIE: &str = "phext_oauth";
const SESSION_TTL: i64 = 60 * 60 * 24 * 14;
const PENDING_TTL: i64 = 600;
const AUTH_WINDOW: Duration = Duration::from_secs(10 * 60);
const AUTH_PER_IP: usize = 20;
pub const TEST_CSRF: &str = "csrf-test-token";

type HmacSha256 = Hmac<Sha256>;

// ── Identity ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Identity {
    pub who: String,
    pub handle: String,
    pub name: String,
    pub email: String,
}

pub trait IdentitySource: Send + Sync {
    fn exchange(
        &self,
        provider: &str,
        code: &str,
        verifier: &str,
        redirect: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<Identity, String>;
}

struct HttpSource;

impl IdentitySource for HttpSource {
    fn exchange(
        &self,
        provider: &str,
        code: &str,
        verifier: &str,
        redirect: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<Identity, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .redirects(0)
            .build();
        match provider {
            "github" => github_identity(&agent, code, verifier, redirect, client_id, client_secret),
            "google" => google_identity(&agent, code, verifier, redirect, client_id, client_secret),
            _ => Err("the provider did not complete sign-in.".into()),
        }
    }
}

struct ScriptedSource;

impl IdentitySource for ScriptedSource {
    fn exchange(
        &self,
        provider: &str,
        code: &str,
        _verifier: &str,
        _redirect: &str,
        _client_id: &str,
        _client_secret: &str,
    ) -> Result<Identity, String> {
        if code != "ok" {
            return Err("the provider did not complete sign-in.".into());
        }
        match provider {
            "github" => Ok(Identity {
                who: "github:100".into(),
                handle: "octocat".into(),
                name: "The Octocat".into(),
                email: "octo@example.com".into(),
            }),
            "google" => Ok(Identity {
                who: "google:sub-1".into(),
                handle: "ada".into(),
                name: "Ada Example".into(),
                email: "ada@example.com".into(),
            }),
            _ => Err("the provider did not complete sign-in.".into()),
        }
    }
}

// ── Oauth ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ProviderKeys {
    id: String,
    secret: String,
}

pub struct Oauth {
    /// Public base URL, origin plus an optional path prefix
    /// (e.g. `https://mirrorborn.us/editor`). No trailing slash.
    base: String,
    session_secret: String,
    cookie_secure: bool,
    github: Option<ProviderKeys>,
    google: Option<ProviderKeys>,
    source: Arc<dyn IdentitySource>,
}

impl std::fmt::Debug for Oauth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Oauth")
    }
}

#[derive(Clone)]
pub struct Session {
    pub who: String,
    pub handle: String,
    pub name: String,
    pub email: String,
    pub csrf: String,
}

#[derive(Serialize, Deserialize)]
struct SessionBody {
    who: String,
    handle: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    csrf: String,
    exp: i64,
}

#[derive(Serialize, Deserialize)]
struct PendingBody {
    provider: String,
    state: String,
    verifier: String,
    next: String,
    exp: i64,
}

struct Pending {
    provider: String,
    state: String,
    verifier: String,
    next: String,
}

impl Oauth {
    pub fn from_env() -> Self {
        Self::from_parts(
            &std::env::var("PHEXT_PUBLIC_URL").unwrap_or_default(),
            &std::env::var("PHEXT_SESSION_SECRET").unwrap_or_default(),
            std::env::var("PHEXT_COOKIE_SECURE").unwrap_or_default().trim() == "1",
            &std::env::var("GITHUB_CLIENT_ID").unwrap_or_default(),
            &std::env::var("GITHUB_CLIENT_SECRET").unwrap_or_default(),
            &std::env::var("GOOGLE_CLIENT_ID").unwrap_or_default(),
            &std::env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default(),
            Arc::new(HttpSource),
        )
    }

    pub fn test_ready() -> Self {
        Self::from_parts(
            "http://127.0.0.1:8080",
            "test-session-secret-must-be-32chars",
            true,
            "github-client-id",
            "github-client-secret",
            "google-client-id",
            "google-client-secret",
            Arc::new(ScriptedSource),
        )
    }

    pub fn closed() -> Self {
        Self::from_parts("", "", false, "", "", "", "", Arc::new(ScriptedSource))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        base: &str,
        session_secret: &str,
        cookie_secure: bool,
        github_id: &str,
        github_secret: &str,
        google_id: &str,
        google_secret: &str,
        source: Arc<dyn IdentitySource>,
    ) -> Self {
        let base = base.trim().trim_end_matches('/').to_string();
        let base = if valid_base(&base) { base } else { String::new() };
        Self {
            base,
            session_secret: session_secret.trim().to_string(),
            cookie_secure,
            github: provider(github_id, github_secret),
            google: provider(google_id, google_secret),
            source,
        }
    }

    pub fn ready(&self) -> bool {
        real_session_secret(&self.session_secret)
            && !self.base.is_empty()
            && (self.github.is_some() || self.google.is_some())
    }

    pub fn providers(&self) -> Vec<&'static str> {
        if !self.ready() {
            return Vec::new();
        }
        let mut names = Vec::new();
        if self.github.is_some() {
            names.push("github");
        }
        if self.google.is_some() {
            names.push("google");
        }
        names
    }

    pub fn cookie_secure(&self) -> bool {
        self.cookie_secure
    }

    /// Returns the provider URL to redirect to and the sealed pending cookie.
    pub fn authorize_url(&self, provider_name: &str, next: &str) -> Result<(String, String), String> {
        if !self.ready() {
            return Err("Sign-in is not set up yet.".into());
        }
        let keys = self
            .keys(provider_name)
            .ok_or_else(|| "Sign-in is not set up yet.".to_string())?;
        let verifier = random_token(32)?;
        let state = random_token(16)?;
        let challenge = pkce_challenge(&verifier);
        let redirect = self.redirect_uri(provider_name);
        let next = safe_next(next);
        let url = match provider_name {
            "github" => format!(
                "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read:user&state={}&code_challenge={}&code_challenge_method=S256",
                form_encode(&keys.id),
                form_encode(&redirect),
                form_encode(&state),
                form_encode(&challenge),
            ),
            "google" => format!(
                "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256&prompt=select_account",
                form_encode(&keys.id),
                form_encode(&redirect),
                form_encode("openid email profile"),
                form_encode(&state),
                form_encode(&challenge),
            ),
            _ => return Err("Sign-in is not set up yet.".into()),
        };
        let pending = self.seal(&PendingBody {
            provider: provider_name.to_string(),
            state,
            verifier,
            next,
            exp: now() + PENDING_TTL,
        })?;
        Ok((url, pending))
    }

    /// Completes the flow. Returns the identity, the sealed session cookie,
    /// and the absolute URL to send the browser back to.
    pub fn finish(
        &self,
        provider_name: &str,
        code: &str,
        state: &str,
        pending_cookie: &str,
    ) -> Result<(Identity, String, String), String> {
        if !self.ready() {
            return Err("Sign-in is not set up yet.".into());
        }
        let pending = self.open_pending(pending_cookie)?;
        if pending.provider != provider_name || !token_eq(&pending.state, state) {
            return Err("Sign-in did not complete.".into());
        }
        let keys = self
            .keys(provider_name)
            .ok_or_else(|| "Sign-in is not set up yet.".to_string())?;
        let identity = self.source.exchange(
            provider_name,
            code,
            &pending.verifier,
            &self.redirect_uri(provider_name),
            &keys.id,
            &keys.secret,
        )?;
        if !valid_who(&identity.who) || identity.handle.is_empty() {
            return Err("Sign-in did not complete.".into());
        }
        let csrf = random_token(16)?;
        let session = self.seal(&SessionBody {
            who: identity.who.clone(),
            handle: identity.handle.clone(),
            name: display_name(&identity.name, &identity.handle),
            email: clean_email(&identity.email),
            csrf,
            exp: now() + SESSION_TTL,
        })?;
        Ok((identity, session, format!("{}{}", self.base, pending.next)))
    }

    pub fn read_session(&self, cookie_header: &str) -> Option<Session> {
        if !real_session_secret(&self.session_secret) {
            return None;
        }
        let raw = cookie_value(cookie_header, SESSION_COOKIE)?;
        let body = self.open::<SessionBody>(&raw).ok()?;
        if body.exp < now() || body.csrf.is_empty() || !valid_who(&body.who) {
            return None;
        }
        let handle = clean_handle(&body.handle);
        Some(Session {
            who: body.who,
            handle: handle.clone(),
            name: display_name(&body.name, &handle),
            email: clean_email(&body.email),
            csrf: body.csrf,
        })
    }

    pub fn csrf_matches(&self, session: &Session, header: &str) -> bool {
        !session.csrf.is_empty() && token_eq(&session.csrf, header.trim())
    }

    pub fn test_session_cookie() -> String {
        Self::test_ready()
            .seal(&SessionBody {
                who: "github:100".into(),
                handle: "octocat".into(),
                name: "The Octocat".into(),
                email: "octo@example.com".into(),
                csrf: TEST_CSRF.into(),
                exp: now() + 3600,
            })
            .expect("test session")
    }

    fn keys(&self, provider_name: &str) -> Option<&ProviderKeys> {
        match provider_name {
            "github" => self.github.as_ref(),
            "google" => self.google.as_ref(),
            _ => None,
        }
    }

    fn redirect_uri(&self, provider_name: &str) -> String {
        format!("{}/api/auth/{provider_name}/callback", self.base)
    }

    fn seal<T: Serialize>(&self, body: &T) -> Result<String, String> {
        let json = serde_json::to_vec(body).map_err(|_| "Sign-in did not complete.".to_string())?;
        let payload = base64url(&json);
        let sig = sign(&self.session_secret, &payload)?;
        Ok(format!("{payload}.{sig}"))
    }

    fn open<T: for<'de> Deserialize<'de>>(&self, token: &str) -> Result<T, String> {
        let (payload, sig) = token
            .split_once('.')
            .ok_or_else(|| "Sign-in did not complete.".to_string())?;
        if payload.len() > 4000 || sig.len() > 200 || !verify(&self.session_secret, payload, sig) {
            return Err("Sign-in did not complete.".into());
        }
        let bytes = base64url_decode(payload).ok_or_else(|| "Sign-in did not complete.".to_string())?;
        serde_json::from_slice(&bytes).map_err(|_| "Sign-in did not complete.".into())
    }

    fn open_pending(&self, token: &str) -> Result<Pending, String> {
        let body = self.open::<PendingBody>(token)?;
        if body.exp < now() {
            return Err("Sign-in did not complete.".into());
        }
        Ok(Pending {
            provider: body.provider,
            state: body.state,
            verifier: body.verifier,
            next: safe_next(&body.next),
        })
    }
}

// ── Editors allowlist ──────────────────────────────────────────────────

enum Editor {
    Who(String),
    Email(String),
}

/// Accounts allowed to use the editor once signed in.
#[derive(Default)]
pub struct Editors {
    entries: Vec<Editor>,
}

impl Editors {
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("PHEXT_EDITORS").unwrap_or_default())
    }

    pub fn parse(raw: &str) -> Self {
        let mut entries = Vec::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() || part.len() > 120 {
                continue;
            }
            if part.contains('@') {
                let email = clean_email(part);
                if !email.is_empty() {
                    entries.push(Editor::Email(email));
                }
            } else if valid_who(part) {
                entries.push(Editor::Who(part.to_string()));
            }
        }
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn matches(&self, who: &str, email: &str) -> bool {
        let email = email.trim().to_lowercase();
        self.entries.iter().any(|entry| match entry {
            Editor::Who(id) => id == who,
            Editor::Email(expected) => !email.is_empty() && expected == &email,
        })
    }
}

// ── Limiter ────────────────────────────────────────────────────────────

/// In-memory cap on sign-in attempts per client address, per ten minutes.
pub struct Limiter {
    cap: usize,
    hits: Mutex<Vec<(String, Instant)>>,
}

impl Limiter {
    pub fn standard() -> Self {
        Self::with_cap(AUTH_PER_IP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self { cap, hits: Mutex::new(Vec::new()) }
    }

    pub fn hit(&self, key: &str) -> Result<(), ()> {
        let mut hits = self.hits.lock().map_err(|_| ())?;
        let now = Instant::now();
        hits.retain(|(_, at)| now.saturating_duration_since(*at) < AUTH_WINDOW);
        if hits.iter().filter(|(k, _)| k == key).count() >= self.cap {
            return Err(());
        }
        hits.push((key.to_string(), now));
        Ok(())
    }
}

// ── Gate ───────────────────────────────────────────────────────────────

/// A complete HTTP response for the hand-rolled servers to write out.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "ok": false, "error": message }))
    }

    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            content_type: "text/plain",
            headers: vec![("Location", location)],
            body: Vec::new(),
        }
    }

    fn cookie(mut self, value: String) -> Self {
        self.headers.push(("Set-Cookie", value));
        self
    }

    /// Serialize as an HTTP/1.1 response with `Connection: close`.
    pub fn write_to(&self, out: &mut impl std::io::Write) {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
            self.status,
            status_text(self.status),
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = out.write_all(head.as_bytes());
        let _ = out.write_all(&self.body);
        let _ = out.flush();
    }
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

/// Access control for the editor servers: bearer token, OAuth session, or
/// open when neither is configured.
pub struct Gate {
    token: Option<String>,
    oauth: Oauth,
    editors: Editors,
    limits: Limiter,
}

impl Gate {
    pub fn new(token: Option<String>, oauth: Oauth, editors: Editors) -> Self {
        let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        Self { token, oauth, editors, limits: Limiter::standard() }
    }

    pub fn from_env(token: Option<String>) -> Self {
        Self::new(token, Oauth::from_env(), Editors::from_env())
    }

    /// True when some credential is required for the API.
    pub fn locked(&self) -> bool {
        self.token.is_some() || self.oauth.ready()
    }

    pub fn oauth(&self) -> &Oauth {
        &self.oauth
    }

    pub fn editors(&self) -> &Editors {
        &self.editors
    }

    /// Startup lines describing how the API is protected.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.token.is_some() {
            lines.push("auth: token accepted".to_string());
        }
        if self.oauth.ready() {
            lines.push(format!(
                "auth: sign-in via {} ({} editors)",
                self.oauth.providers().join(", "),
                self.editors.len()
            ));
            if self.editors.is_empty() {
                lines.push("auth: PHEXT_EDITORS is empty; signed-in accounts cannot edit".to_string());
            }
        }
        if !self.locked() {
            lines.push("auth: open (no token, sign-in not configured)".to_string());
        }
        lines
    }

    /// Handles `/api/session` and `/api/auth/*`. Returns None for other paths.
    pub fn route(&self, method: &str, path: &str, headers: &HashMap<String, String>) -> Option<Reply> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let reply = match (method, route) {
            ("GET", "/api/session") => self.session_view(headers),
            ("POST", "/api/auth/logout") => self.logout(headers),
            ("GET", p) if p.starts_with("/api/auth/") => {
                let rest = &p["/api/auth/".len()..];
                let query = query_map(query);
                match rest.split_once('/') {
                    Some((provider, "callback")) => self.callback(headers, provider, &query),
                    None => self.start(headers, rest, query.get("next").map(String::as_str).unwrap_or("/")),
                    _ => Reply::error(404, "not found"),
                }
            }
            _ => return None,
        };
        Some(reply)
    }

    /// Decides whether a non-auth request may proceed.
    pub fn admit(&self, method: &str, path: &str, headers: &HashMap<String, String>) -> Result<(), Reply> {
        let route = path.split('?').next().unwrap_or(path);
        if route == "/" || route == "/index.html" || !self.locked() {
            return Ok(());
        }
        if let Some(ref token) = self.token {
            let auth = headers.get("authorization").map(String::as_str).unwrap_or("");
            let given = if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
                &auth[7..]
            } else {
                auth
            };
            if token_eq(token, given.trim()) {
                return Ok(());
            }
        }
        let Some(session) = self.oauth.read_session(&cookies(headers)) else {
            return Err(Reply::error(401, "unauthorized"));
        };
        if !self.editors.matches(&session.who, &session.email) {
            return Err(Reply::error(403, "This account is not on the editor list."));
        }
        if method != "GET" && method != "HEAD" {
            let csrf = headers.get("x-csrf-token").map(String::as_str).unwrap_or("");
            if !self.oauth.csrf_matches(&session, csrf) {
                return Err(Reply::error(403, "That request did not match this sign-in."));
            }
        }
        Ok(())
    }

    fn session_view(&self, headers: &HashMap<String, String>) -> Reply {
        let session = self.oauth.read_session(&cookies(headers));
        let editor = session
            .as_ref()
            .map(|s| self.editors.matches(&s.who, &s.email))
            .unwrap_or(false);
        let field = |f: fn(&Session) -> &str| session.as_ref().map(f).unwrap_or("").to_string();
        Reply::json(200, json!({
            "locked": self.locked(),
            "token": self.token.is_some(),
            "ready": self.oauth.ready(),
            "providers": self.oauth.providers(),
            "signed_in": session.is_some(),
            "editor": editor,
            "who": field(|s| &s.who),
            "handle": field(|s| &s.handle),
            "name": field(|s| &s.name),
            "csrf": field(|s| &s.csrf),
        }))
    }

    fn guard_auth(&self, headers: &HashMap<String, String>) -> Result<(), Reply> {
        if !self.oauth.ready() {
            return Err(Reply::error(403, "Sign-in is not set up yet."));
        }
        if self.limits.hit(&client_key(headers)).is_err() {
            return Err(Reply::error(429, "Too many requests."));
        }
        Ok(())
    }

    fn start(&self, headers: &HashMap<String, String>, provider: &str, next: &str) -> Reply {
        if let Err(reply) = self.guard_auth(headers) {
            return reply;
        }
        match self.oauth.authorize_url(provider, next) {
            Ok((url, pending)) => Reply::redirect(url)
                .cookie(cookie_header(PENDING_COOKIE, &pending, PENDING_TTL, self.oauth.cookie_secure())),
            Err(message) => Reply::error(403, &message),
        }
    }

    fn callback(&self, headers: &HashMap<String, String>, provider: &str, query: &HashMap<String, String>) -> Reply {
        if let Err(reply) = self.guard_auth(headers) {
            return reply;
        }
        let secure = self.oauth.cookie_secure();
        let code = query.get("code").map(String::as_str).unwrap_or("");
        let state = query.get("state").map(String::as_str).unwrap_or("");
        let pending = cookie_value(&cookies(headers), PENDING_COOKIE).unwrap_or_default();
        if query.contains_key("error") || code.is_empty() || state.is_empty() {
            return auth_failed(secure);
        }
        match self.oauth.finish(provider, code, state, &pending) {
            Ok((_identity, session, next)) => Reply::redirect(next)
                .cookie(cookie_header(SESSION_COOKIE, &session, SESSION_TTL, secure))
                .cookie(cookie_header(PENDING_COOKIE, "", 0, secure)),
            Err(_) => auth_failed(secure),
        }
    }

    fn logout(&self, headers: &HashMap<String, String>) -> Reply {
        let Some(session) = self.oauth.read_session(&cookies(headers)) else {
            return Reply::error(401, "Sign in to do that.");
        };
        let csrf = headers.get("x-csrf-token").map(String::as_str).unwrap_or("");
        if !self.oauth.csrf_matches(&session, csrf) {
            return Reply::error(403, "That request did not match this sign-in.");
        }
        Reply::json(200, json!({ "ok": true }))
            .cookie(cookie_header(SESSION_COOKIE, "", 0, self.oauth.cookie_secure()))
    }
}

fn auth_failed(secure: bool) -> Reply {
    Reply {
        status: 400,
        content_type: "text/html; charset=utf-8",
        headers: Vec::new(),
        body: b"<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>phext</title></head><body><p>Sign-in did not complete. <a href=\"./\">Back</a></p></body></html>".to_vec(),
    }
    .cookie(cookie_header(PENDING_COOKIE, "", 0, secure))
}

// ── Helpers ────────────────────────────────────────────────────────────

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cookies(headers: &HashMap<String, String>) -> String {
    headers.get("cookie").cloned().unwrap_or_default()
}

fn client_key(headers: &HashMap<String, String>) -> String {
    for name in ["x-real-ip", "x-forwarded-for"] {
        if let Some(value) = headers.get(name) {
            let value = value.split(',').next().unwrap_or("").trim();
            if !value.is_empty()
                && value.len() <= 64
                && value.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':'))
            {
                return value.to_string();
            }
        }
    }
    "local".into()
}

pub fn cookie_header(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}")
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if key.trim() == name {
            let value = value.trim();
            if value.is_empty() || value.len() > 4500 {
                return None;
            }
            return Some(value.to_string());
        }
    }
    None
}

fn query_map(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn provider(id: &str, secret: &str) -> Option<ProviderKeys> {
    let id = id.trim();
    let secret = secret.trim();
    if !real_client(id) || !real_client(secret) {
        return None;
    }
    Some(ProviderKeys { id: id.to_string(), secret: secret.to_string() })
}

fn real_client(value: &str) -> bool {
    value.len() >= 8
        && !value.eq_ignore_ascii_case("replace-me")
        && !value.eq_ignore_ascii_case("changeme")
        && value.chars().all(|ch| ch.is_ascii_graphic())
}

fn real_session_secret(value: &str) -> bool {
    value.len() >= 32
        && !value.eq_ignore_ascii_case("replace-me")
        && !value.eq_ignore_ascii_case("changeme")
        && value.chars().all(|ch| ch.is_ascii_graphic())
}

/// `http(s)://host[:port][/path]`, no query, fragment, credentials, or backslash.
fn valid_base(base: &str) -> bool {
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"));
    let Some(rest) = rest else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty()
        && !rest.contains('@')
        && !rest.contains('?')
        && !rest.contains('#')
        && !rest.contains('\\')
        && !rest.contains("//")
}

/// Keeps the post-sign-in redirect on this site: a path, optionally with a
/// `#coordinate` fragment.
pub fn safe_next(next: &str) -> String {
    let next = next.trim();
    if next.len() > 200
        || !next.starts_with('/')
        || next.starts_with("//")
        || next.contains('\\')
        || next.contains(':')
        || next.chars().any(|ch| ch.is_control() || ch == ' ')
    {
        return "/".into();
    }
    next.to_string()
}

pub fn valid_who(who: &str) -> bool {
    let Some((kind, id)) = who.split_once(':') else {
        return false;
    };
    if id.is_empty() || id.len() > 80 {
        return false;
    }
    let id_ok = id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    id_ok
        && match kind {
            "github" => id.chars().all(|ch| ch.is_ascii_digit()),
            "google" => true,
            _ => false,
        }
}

pub fn display_name(raw: &str, fallback: &str) -> String {
    let flat = raw
        .split(|ch: char| ch.is_whitespace() || ch.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let name: String = flat.chars().take(80).collect();
    let name = name.trim().to_string();
    if !name.is_empty() {
        return name;
    }
    let fallback = fallback.trim();
    if fallback.is_empty() { "reader".into() } else { fallback.to_string() }
}

pub fn clean_email(raw: &str) -> String {
    let email = raw.trim().to_lowercase();
    if !(3..=120).contains(&email.len()) {
        return String::new();
    }
    let Some((local, host)) = email.split_once('@') else {
        return String::new();
    };
    if local.is_empty()
        || host.is_empty()
        || !host.contains('.')
        || email.contains(':')
        || !email
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '-' | '_' | '+'))
    {
        return String::new();
    }
    email
}

pub fn clean_handle(raw: &str) -> String {
    let out: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .take(39)
        .collect();
    if out.is_empty() { "reader".into() } else { out }
}

/// Constant-time comparison; an empty expected value never matches.
pub fn token_eq(expected: &str, given: &str) -> bool {
    let left = expected.as_bytes();
    let right = given.as_bytes();
    if left.is_empty() {
        return false;
    }
    let mut diff = left.len() ^ right.len();
    for (i, byte) in left.iter().enumerate() {
        diff |= (*byte ^ right.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

fn random_token(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|_| "Sign-in did not complete.".to_string())?;
    Ok(base64url(&buf))
}

fn sign(secret: &str, payload: &str) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| "Sign-in did not complete.".to_string())?;
    mac.update(payload.as_bytes());
    Ok(base64url(&mac.finalize().into_bytes()))
}

fn verify(secret: &str, payload: &str, sig: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(payload.as_bytes());
    let Some(bytes) = base64url_decode(sig) else {
        return false;
    };
    mac.verify_slice(&bytes).is_ok()
}

fn github_identity(
    agent: &ureq::Agent,
    code: &str,
    verifier: &str,
    redirect: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<Identity, String> {
    let body = json!({
        "client_id": client_id,
        "client_secret": client_secret,
        "code": code,
        "redirect_uri": redirect,
        "code_verifier": verifier,
    });
    let token = post(agent, "https://github.com/login/oauth/access_token", &body.to_string(), "application/json")?;
    let access = access_token(&token)?;
    let user = get_json(agent, "https://api.github.com/user", "application/vnd.github+json", &access)?;
    let id = user
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "the provider did not complete sign-in.".to_string())?;
    let handle = clean_handle(user.get("login").and_then(|v| v.as_str()).unwrap_or(""));
    let profile = user.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let email = user.get("email").and_then(|v| v.as_str()).unwrap_or("");
    Ok(Identity {
        who: format!("github:{id}"),
        handle: handle.clone(),
        name: display_name(profile, &handle),
        email: clean_email(email),
    })
}

fn google_identity(
    agent: &ureq::Agent,
    code: &str,
    verifier: &str,
    redirect: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<Identity, String> {
    let form = format!(
        "code={}&client_id={}&client_secret={}&redirect_uri={}&grant_type=authorization_code&code_verifier={}",
        form_encode(code),
        form_encode(client_id),
        form_encode(client_secret),
        form_encode(redirect),
        form_encode(verifier),
    );
    let token = post(agent, "https://oauth2.googleapis.com/token", &form, "application/x-www-form-urlencoded")?;
    let access = access_token(&token)?;
    let user = get_json(agent, "https://openidconnect.googleapis.com/v1/userinfo", "application/json", &access)?;
    let sub = user.get("sub").and_then(|v| v.as_str()).unwrap_or("");
    if !valid_who(&format!("google:{sub}")) {
        return Err("the provider did not complete sign-in.".into());
    }
    // Only trust the email for the allowlist when Google has verified it.
    let verified = user.get("email_verified").and_then(|v| v.as_bool()).unwrap_or(false);
    let email = if verified { user.get("email").and_then(|v| v.as_str()).unwrap_or("") } else { "" };
    let profile = user.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let local = email.split('@').next().unwrap_or("");
    let handle = clean_handle(if local.is_empty() { profile } else { local });
    Ok(Identity {
        who: format!("google:{sub}"),
        handle: handle.clone(),
        name: display_name(profile, &handle),
        email: clean_email(email),
    })
}

fn access_token(raw: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| "the provider did not complete sign-in.")?;
    parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "the provider did not complete sign-in.".to_string())
}

fn get_json(agent: &ureq::Agent, url: &str, accept: &str, access: &str) -> Result<serde_json::Value, String> {
    let body = agent
        .get(url)
        .set("Accept", accept)
        .set("User-Agent", "phext-lattice")
        .set("Authorization", &format!("Bearer {access}"))
        .call()
        .map_err(|_| "the provider did not complete sign-in.")?
        .into_string()
        .map_err(|_| "the provider did not complete sign-in.")?;
    serde_json::from_str(&body).map_err(|_| "the provider did not complete sign-in.".into())
}

fn post(agent: &ureq::Agent, url: &str, body: &str, content_type: &str) -> Result<String, String> {
    agent
        .post(url)
        .set("Accept", "application/json")
        .set("User-Agent", "phext-lattice")
        .set("Content-Type", content_type)
        .send_string(body)
        .map_err(|_| "the provider did not complete sign-in.")?
        .into_string()
        .map_err(|_| "the provider did not complete sign-in.".into())
}

fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(byte: u8) -> Option<u32> {
        B64.iter().position(|b| *b == byte).map(|p| p as u32)
    }
    let bytes = input.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            n |= val(*b)? << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn gate(token: Option<&str>, editors: &str) -> Gate {
        Gate::new(token.map(str::to_string), Oauth::test_ready(), Editors::parse(editors))
    }

    fn header<'a>(reply: &'a Reply, name: &str) -> Vec<&'a str> {
        reply.headers.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect()
    }

    #[test]
    fn pkce_matches_the_spec_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn base64url_roundtrips_every_tail_length() {
        for len in 0..10 {
            let data: Vec<u8> = (0..len as u8).map(|b| b.wrapping_mul(37)).collect();
            let enc = base64url(&data);
            if len == 0 {
                assert!(enc.is_empty());
            } else {
                assert_eq!(base64url_decode(&enc).unwrap(), data);
            }
        }
        assert_eq!(base64url(b"hi?"), "aGk_");
    }

    #[test]
    fn a_tampered_session_does_not_open() {
        let cookie = Oauth::test_session_cookie();
        let oauth = Oauth::test_ready();
        assert!(oauth.read_session(&format!("phext_session={cookie}")).is_some());
        let mut flipped = cookie.clone();
        let end = flipped.pop().unwrap();
        flipped.push(if end == 'A' { 'B' } else { 'A' });
        assert!(oauth.read_session(&format!("phext_session={flipped}")).is_none());
    }

    #[test]
    fn hostile_next_urls_stay_on_the_site() {
        assert_eq!(safe_next("/#1.1.1/1.1.1/1.1.1"), "/#1.1.1/1.1.1/1.1.1");
        assert_eq!(safe_next("https://evil.example"), "/");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("/\\evil"), "/");
    }

    #[test]
    fn base_urls_may_carry_a_path_prefix() {
        assert!(valid_base("https://mirrorborn.us/editor"));
        assert!(valid_base("http://127.0.0.1:8080"));
        assert!(!valid_base("https://user@evil.example"));
        assert!(!valid_base("ftp://example.com"));
        assert!(!valid_base("https://example.com/?x"));
    }

    #[test]
    fn empty_client_ids_leave_sign_in_closed() {
        let oauth = Oauth::closed();
        assert!(!oauth.ready());
        assert!(oauth.providers().is_empty());
        assert!(oauth.authorize_url("github", "/").is_err());
    }

    #[test]
    fn editors_match_by_account_or_email() {
        let editors = Editors::parse("github:100, Ada@Example.com, bogus, twitter:1");
        assert_eq!(editors.len(), 2);
        assert!(editors.matches("github:100", ""));
        assert!(editors.matches("google:sub-1", "ada@example.com"));
        assert!(!editors.matches("github:101", ""));
    }

    #[test]
    fn an_open_gate_admits_everything() {
        let gate = Gate::new(None, Oauth::closed(), Editors::default());
        assert!(!gate.locked());
        assert!(gate.admit("POST", "/api/update", &headers(&[])).is_ok());
    }

    #[test]
    fn the_bearer_token_still_admits() {
        let gate = Gate::new(Some("s3cret-token".into()), Oauth::closed(), Editors::default());
        assert!(gate.admit("GET", "/", &headers(&[])).is_ok());
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[])).unwrap_err().status, 401);
        assert!(gate.admit("GET", "/api/nav", &headers(&[("authorization", "Bearer s3cret-token")])).is_ok());
        assert!(gate.admit("GET", "/api/nav", &headers(&[("authorization", "Bearer nope")])).is_err());
    }

    #[test]
    fn a_session_needs_the_editor_list_and_csrf_for_writes() {
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        let outsider = gate(None, "github:999");
        assert_eq!(outsider.admit("GET", "/api/nav", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);

        let gate = gate(None, "github:100");
        assert!(gate.admit("GET", "/api/zoom/y/1.1.1?x=1", &headers(&[("cookie", &cookie)])).is_ok());
        assert_eq!(gate.admit("POST", "/api/update", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);
        assert!(gate
            .admit("POST", "/api/update", &headers(&[("cookie", &cookie), ("x-csrf-token", TEST_CSRF)]))
            .is_ok());
    }

    #[test]
    fn the_full_sign_in_flow_sets_a_session() {
        let gate = gate(None, "github:100");
        let start = gate.route("GET", "/api/auth/github?next=%2F%231.1.1%2F1.1.1%2F1.1.2", &headers(&[])).unwrap();
        assert_eq!(start.status, 302);
        let location = header(&start, "Location")[0];
        assert!(location.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(location.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fapi%2Fauth%2Fgithub%2Fcallback"));
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap();
        let pending = header(&start, "Set-Cookie")[0].split(';').next().unwrap().to_string();

        let back = gate
            .route("GET", &format!("/api/auth/github/callback?code=ok&state={state}"), &headers(&[("cookie", &pending)]))
            .unwrap();
        assert_eq!(back.status, 302);
        assert_eq!(header(&back, "Location")[0], "http://127.0.0.1:8080/#1.1.1/1.1.1/1.1.2");
        let session = header(&back, "Set-Cookie")[0].split(';').next().unwrap().to_string();
        assert!(session.starts_with("phext_session="));
        assert!(gate.admit("GET", "/api/nav", &headers(&[("cookie", &session)])).is_ok());

        let wrong_state = gate
            .route("GET", "/api/auth/github/callback?code=ok&state=forged", &headers(&[("cookie", &pending)]))
            .unwrap();
        assert_eq!(wrong_state.status, 400);
    }

    #[test]
    fn logout_needs_csrf_and_clears_the_cookie() {
        let gate = gate(None, "github:100");
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        assert_eq!(gate.route("POST", "/api/auth/logout", &headers(&[("cookie", &cookie)])).unwrap().status, 403);
        let out = gate
            .route("POST", "/api/auth/logout", &headers(&[("cookie", &cookie), ("x-csrf-token", TEST_CSRF)]))
            .unwrap();
        assert_eq!(out.status, 200);
        assert!(header(&out, "Set-Cookie")[0].contains("Max-Age=0"));
    }

    #[test]
    fn sign_in_attempts_are_capped_per_address() {
        let limiter = Limiter::with_cap(2);
        assert!(limiter.hit("203.0.113.8").is_ok());
        assert!(limiter.hit("203.0.113.8").is_ok());
        assert!(limiter.hit("203.0.113.8").is_err());
        assert!(limiter.hit("203.0.113.9").is_ok());
    }

    #[test]
    fn query_values_are_percent_decoded() {
        let q = query_map("next=%2F%23a&code=ok&bad=%zz");
        assert_eq!(q["next"], "/#a");
        assert_eq!(q["code"], "ok");
        assert_eq!(q["bad"], "%zz");
    }
}
