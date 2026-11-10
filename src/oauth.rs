/// GitHub, Google, and X sign-in for the web editors.
///
/// X exists for the claim flow: an X sign-in proves control of an X
/// (Twitter) account, and the returned `username` is the handle the
/// wavefront map is keyed by — `Identity.handle` for provider `x` is the
/// X username verbatim (underscores kept; `clean_handle` would eat them,
/// so `x_identity` uses `clean_twitter_handle`).
///
/// Ported from phext-lanes. The session is an HttpOnly cookie signed with
/// `PHEXT_SESSION_SECRET`. The authorization-code flow uses PKCE and a
/// `state` value kept in a second short-lived cookie. Writes (any POST)
/// also need the session's CSRF token in `x-csrf-token`.
///
/// When `PHEXT_EDITORS` is set (comma-separated `github:<id>`, `google:<sub>`,
/// or email), only those accounts get in; when it is empty, any signed-in
/// account does.
///
/// The editor list can also grow without the operator: accounts admitted
/// at runtime (an editor accepting a submission in phext-edit) are added
/// live and appended to the editors file — `PHEXT_EDITORS_FILE` when set,
/// otherwise a default the server wires in — so they survive restarts.
/// `PHEXT_EDITORS` remains the bootstrap list.
///
/// `Gate` is the piece the HTTP servers use: `route()` answers the
/// `/api/session` and `/api/auth/*` endpoints, `admit()` decides whether
/// any other request may proceed.
///
/// `Gate::enable_local` is the phext-edit `--local` switch. It stays off
/// unless that method is called: no environment variable and no request
/// header turns it on. A forwarded request (proxy headers, or a Host that
/// is not loopback) does not receive the bypass.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// The Mutable SSO assertion's whole life: long enough for one redirect
/// chain (Mutable → here → sign-in → back), short enough that a logged
/// copy is worthless almost immediately.
const SSO_TTL: i64 = 120;
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
            "x" => x_identity(&agent, code, verifier, redirect, client_id, client_secret),
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
        if code != "ok" && code != "ok-new" && code != "ok-owner" {
            return Err("the provider did not complete sign-in.".into());
        }
        match provider {
            "github" if code == "ok" => Ok(Identity {
                who: "github:100".into(),
                handle: "octocat".into(),
                name: "The Octocat".into(),
                email: "octo@example.com".into(),
            }),
            "google" if code == "ok" => Ok(Identity {
                who: "google:sub-1".into(),
                handle: "ada".into(),
                name: "Ada Example".into(),
                email: "ada@example.com".into(),
            }),
            // The scripted X user's handle is placed in the wavefront
            // snapshot on purpose: claim tests exercise the found tier
            // end to end without a live X app.
            "x" if code == "ok" => Ok(Identity {
                who: "x:1001".into(),
                handle: "jaivinwylde".into(),
                name: "Scripted X User".into(),
                email: String::new(),
            }),
            // An X account the snapshot does not hold: join/dashboard
            // tests exercise the new-era arrival without a live X app.
            "x" if code == "ok-new" => Ok(Identity {
                who: "x:2002".into(),
                handle: "newarrival_x".into(),
                name: "New Arrival".into(),
                email: String::new(),
            }),
            // The owner himself: @wbic16 is placed in the snapshot
            // (the Hearth), and the claim dashboard gates on his X
            // identity as well as on the editors list.
            "x" if code == "ok-owner" => Ok(Identity {
                who: "x:88622".into(),
                handle: "wbic16".into(),
                name: "Will Bickford".into(),
                email: String::new(),
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
    /// Signs SSO assertions for every allowlisted audience (Mutable,
    /// Exocortexia), and nothing else. Deliberately not the session
    /// secret: each audience holds this key to verify assertions, and
    /// a leak over there must not mint edit sessions.
    sso_secret: String,
    /// Where Mutable lives; assertions are redirected to its callback.
    sso_target: String,
    /// Where Exocortexia lives; assertions for that audience are
    /// redirected to its callback. Empty until `PHEXT_SSO_EXOCORTEXIA_URL`
    /// is set: unlike Mutable it has no default, because there is no
    /// established production origin to fall back to — an SSO redirect
    /// target is never guessed.
    sso_exocortexia_target: String,
    /// Where Wishnode lives; assertions for that audience are
    /// redirected to its callback. Empty until `PHEXT_SSO_WISHNODE_URL`
    /// is set: like Exocortexia it has no default, because there is no
    /// established production origin to fall back to — an SSO redirect
    /// target is never guessed.
    sso_wishnode_target: String,
    /// Whether the desktop audience (Phext Notepad) is armed:
    /// `PHEXT_SSO_DESKTOP=1`. A desktop app has no fixed origin to
    /// configure — its callback is a loopback listener it opens per
    /// sign-in attempt — so this audience is armed by an explicit
    /// flag rather than a target URL. Assertions minted for it are
    /// verified again here at the exchange endpoint, never by the
    /// app: a shipped binary cannot hold the SSO secret, so the
    /// secret never leaves this server.
    sso_desktop: bool,
    cookie_secure: bool,
    github: Option<ProviderKeys>,
    google: Option<ProviderKeys>,
    x: Option<ProviderKeys>,
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

/// The SSO assertion: a sealed, single-use statement that an edit
/// session exists, carried to an audience's callback in a redirect and
/// verified there under the SSO secret. It is not a session — the
/// audience mints its own from it, and this server never accepts one
/// back. `aud` names the one audience it was minted for (`mutable`,
/// `exocortexia`, `wishnode`); a consumer must reject any other value,
/// including
/// an assertion minted for a different audience. `ret` is a path on
/// the audience's site, clamped by `safe_next` before sealing; `jti`
/// is the single-use id the audience burns on first sight.
///
/// The one exception to "never accepts one back" is the `desktop`
/// audience (Phext Notepad): a shipped app cannot hold the SSO
/// secret, so its assertions come back here to
/// `POST /api/auth/desktop/exchange`, which verifies them under the
/// secret and burns the `jti` server-side. For that audience `ret`
/// is the app's validated loopback return URL (see
/// `loopback_return`), and `st` carries the app's per-attempt state
/// nonce, sealed at mint and demanded back at the exchange. Web
/// audiences never carry `st` — the field is simply absent from
/// their assertions.
#[derive(Serialize, Deserialize)]
pub struct SsoAssertionBody {
    aud: String,
    who: String,
    handle: String,
    name: String,
    email: String,
    editor: bool,
    ret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    st: Option<String>,
    iat: i64,
    exp: i64,
    jti: String,
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
            &std::env::var("X_CLIENT_ID").unwrap_or_default(),
            &std::env::var("X_CLIENT_SECRET").unwrap_or_default(),
            &std::env::var("PHEXT_SSO_SECRET").unwrap_or_default(),
            &std::env::var("PHEXT_SSO_MUTABLE_URL").unwrap_or_default(),
            &std::env::var("PHEXT_SSO_EXOCORTEXIA_URL").unwrap_or_default(),
            &std::env::var("PHEXT_SSO_WISHNODE_URL").unwrap_or_default(),
            std::env::var("PHEXT_SSO_DESKTOP").unwrap_or_default().trim() == "1",
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
            "x-client-id",
            "x-client-secret",
            "test-sso-secret-must-be-32-chars-minimum",
            "",
            "https://exocortexia.example",
            "https://wishnode.example",
            true,
            Arc::new(ScriptedSource),
        )
    }

    pub fn closed() -> Self {
        Self::from_parts("", "", false, "", "", "", "", "", "", "", "", "", "", false, Arc::new(ScriptedSource))
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
        x_id: &str,
        x_secret: &str,
        sso_secret: &str,
        sso_target: &str,
        sso_exocortexia_target: &str,
        sso_wishnode_target: &str,
        sso_desktop: bool,
        source: Arc<dyn IdentitySource>,
    ) -> Self {
        let base = base.trim().trim_end_matches('/').to_string();
        let base = if valid_base(&base) { base } else { String::new() };
        // Unset means the production Mutable; a set-but-invalid value
        // disables the handoff (empty target) rather than guessing.
        let sso_target = sso_target.trim().trim_end_matches('/').to_string();
        let sso_target = if sso_target.is_empty() {
            "https://mutable.phext.io".to_string()
        } else if valid_base(&sso_target) {
            sso_target
        } else {
            String::new()
        };
        // Unset or invalid leaves Exocortexia unconfigured (empty
        // target, its endpoint 404s) rather than redirecting anywhere
        // guessed — Mutable's default exists because its origin is
        // established; Exocortexia's is set explicitly at deploy time.
        let sso_exocortexia_target = sso_exocortexia_target.trim().trim_end_matches('/').to_string();
        let sso_exocortexia_target = if valid_base(&sso_exocortexia_target) {
            sso_exocortexia_target
        } else {
            String::new()
        };
        // Unset or invalid leaves Wishnode unconfigured the same way
        // (empty target, its endpoint 404s), for the same reason.
        let sso_wishnode_target = sso_wishnode_target.trim().trim_end_matches('/').to_string();
        let sso_wishnode_target = if valid_base(&sso_wishnode_target) {
            sso_wishnode_target
        } else {
            String::new()
        };
        Self {
            base,
            session_secret: session_secret.trim().to_string(),
            sso_secret: sso_secret.trim().to_string(),
            sso_target,
            sso_exocortexia_target,
            sso_wishnode_target,
            sso_desktop,
            cookie_secure,
            github: provider(github_id, github_secret),
            google: provider(google_id, google_secret),
            x: provider(x_id, x_secret),
            source,
        }
    }

    pub fn ready(&self) -> bool {
        real_session_secret(&self.session_secret)
            && !self.base.is_empty()
            && (self.github.is_some() || self.google.is_some() || self.x.is_some())
    }

    /// True when the Mutable SSO handoff is configured: a real SSO
    /// secret and a valid Mutable target. Independent of `ready()` —
    /// the handoff mints nothing of its own; it rides the session the
    /// providers already made, so it needs no provider keys here.
    pub fn sso_ready(&self) -> bool {
        self.sso_ready_for("mutable")
    }

    /// True when the SSO handoff is configured for one audience: the
    /// shared SSO secret is real and that audience has a target. Every
    /// allowlisted audience rides the same secret and the same sealed
    /// assertion format; only the `aud` claim and the callback origin
    /// differ.
    pub fn sso_ready_for(&self, aud: &str) -> bool {
        real_session_secret(&self.sso_secret) && self.sso_target_for(aud).is_some()
    }

    /// True when the desktop audience is armed: a real SSO secret
    /// and the operator's explicit `PHEXT_SSO_DESKTOP=1`. There is no
    /// target to consult — the app's callback is a per-attempt
    /// loopback listener — so readiness is the secret plus the flag.
    pub fn sso_desktop_ready(&self) -> bool {
        real_session_secret(&self.sso_secret) && self.sso_desktop
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn sso_target(&self) -> &str {
        &self.sso_target
    }

    /// The callback origin for an audience, or None when the audience
    /// is not allowlisted or is allowlisted but unconfigured. Mutable
    /// always has a target (its production origin is the default);
    /// Exocortexia has one only once `PHEXT_SSO_EXOCORTEXIA_URL` is
    /// set, and Wishnode only once `PHEXT_SSO_WISHNODE_URL` is. This
    /// match is the audience allowlist — an audience that is
    /// not named here can never be minted for. The desktop audience
    /// is deliberately absent: it has no fixed origin (its callback
    /// is the app's per-attempt loopback listener), so it is armed
    /// and minted on its own path (`sso_desktop_ready`,
    /// `sso_assertion_desktop`).
    pub fn sso_target_for(&self, aud: &str) -> Option<&str> {
        match aud {
            "mutable" if !self.sso_target.is_empty() => Some(&self.sso_target),
            "exocortexia" if !self.sso_exocortexia_target.is_empty() => {
                Some(&self.sso_exocortexia_target)
            }
            "wishnode" if !self.sso_wishnode_target.is_empty() => {
                Some(&self.sso_wishnode_target)
            }
            _ => None,
        }
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
        if self.x.is_some() {
            names.push("x");
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
            "x" => format!(
                "https://twitter.com/i/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
                form_encode(&keys.id),
                form_encode(&redirect),
                form_encode("tweet.read users.read"),
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
        let (session, _csrf) =
            self.issue_session(&identity.who, &identity.handle, &identity.name, &identity.email)?;
        Ok((identity, session, format!("{}{}", self.base, pending.next)))
    }

    /// Seal a fresh session for an identity that has already been
    /// established — by the provider exchange in `finish`, or by a
    /// verified desktop SSO assertion at the exchange endpoint. This
    /// is the one place session values are made: a fresh CSRF token
    /// and a `SESSION_TTL` seal under the session secret. Returns the
    /// sealed cookie value and the CSRF token inside it (the desktop
    /// exchange hands both to the app; the browser flow learns the
    /// CSRF from `/api/session` instead).
    pub fn issue_session(
        &self,
        who: &str,
        handle: &str,
        name: &str,
        email: &str,
    ) -> Result<(String, String), String> {
        let csrf = random_token(16)?;
        let session = self.seal(&SessionBody {
            who: who.to_string(),
            handle: handle.to_string(),
            name: display_name(name, handle),
            email: clean_email(email),
            csrf: csrf.clone(),
            exp: now() + SESSION_TTL,
        })?;
        Ok((session, csrf))
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
        // X handles keep their underscores (see clean_twitter_handle);
        // every other provider goes through the generic cleaner.
        let handle = if body.who.starts_with("x:") {
            let kept = clean_twitter_handle(&body.handle);
            if kept.is_empty() { clean_handle(&body.handle) } else { kept }
        } else {
            clean_handle(&body.handle)
        };
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

    /// A domain-separated, keyed digest for the MCP OAuth 2.1 store.
    ///
    /// Authorization codes, access tokens, refresh tokens, and client
    /// secrets are never stored in the clear. Each value is represented by
    /// HMAC-SHA256 under the existing session secret, with the caller's
    /// domain label mixed in before the value, so a digest minted for one
    /// kind of credential cannot be replayed as another. No separate OAuth
    /// secret is required: rotating the session secret intentionally
    /// invalidates the stored OAuth credentials along with sessions.
    pub fn mcp_oauth_hash(&self, domain: &str, value: &str) -> Option<String> {
        if !real_session_secret(&self.session_secret) || domain.is_empty() || value.is_empty() {
            return None;
        }
        let mut mac = HmacSha256::new_from_slice(self.session_secret.as_bytes()).ok()?;
        mac.update(b"phext-edit/mcp-oauth21/v1\0");
        mac.update(domain.as_bytes());
        mac.update(b"\0");
        mac.update(value.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        Some(out)
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
            "x" => self.x.as_ref(),
            _ => None,
        }
    }

    fn redirect_uri(&self, provider_name: &str) -> String {
        format!("{}/api/auth/{provider_name}/callback", self.base)
    }

    fn seal<T: Serialize>(&self, body: &T) -> Result<String, String> {
        self.seal_with(&self.session_secret, body)
    }

    /// Seal a body under a chosen secret. The session secret signs the
    /// session and pending cookies; the SSO secret signs Mutable
    /// assertions. The two keys never substitute for each other, which
    /// is what keeps a Mutable-side leak from minting edit sessions.
    fn seal_with<T: Serialize>(&self, secret: &str, body: &T) -> Result<String, String> {
        let json = serde_json::to_vec(body).map_err(|_| "Sign-in did not complete.".to_string())?;
        let payload = base64url(&json);
        let sig = sign(secret, &payload)?;
        Ok(format!("{payload}.{sig}"))
    }

    /// Mint the assertion an audience's callback verifies: who this
    /// session is, whether it is an editor, and the return path on the
    /// audience's site — sealed under the shared SSO secret, alive for
    /// `SSO_TTL` seconds, with a fresh `jti` the audience burns on
    /// first sight. The format is identical for every audience; only
    /// the `aud` claim differs, and an audience outside the allowlist
    /// in `sso_target_for` can never be minted for. The return path is
    /// clamped by `safe_next` here, at the moment of sealing, so no
    /// unsigned return input ever crosses.
    pub fn sso_assertion(
        &self,
        aud: &str,
        session: &Session,
        editor: bool,
        ret: &str,
    ) -> Result<String, String> {
        if !self.sso_ready_for(aud) {
            return Err("Sign-in is not set up yet.".into());
        }
        let iat = now();
        self.seal_with(
            &self.sso_secret,
            &SsoAssertionBody {
                aud: aud.into(),
                who: session.who.clone(),
                handle: session.handle.clone(),
                name: session.name.clone(),
                email: session.email.clone(),
                editor,
                ret: safe_next(ret),
                st: None,
                iat,
                exp: iat + SSO_TTL,
                jti: random_token(16)?,
            },
        )
    }

    /// Mint the desktop audience's assertion. Same sealed format as
    /// every audience, with the two differences that audience exists
    /// for: `ret` is the app's loopback return URL, validated by
    /// `loopback_return` here at the moment of sealing (anything else
    /// is refused, so no unvalidated return ever crosses), and the
    /// app's per-attempt `state` nonce rides sealed as `st`, to be
    /// demanded back at the exchange — an assertion that arrives
    /// anywhere without its attempt's state is not that attempt's
    /// assertion. The caller (`Gate::sso_desktop`) has already
    /// validated the state's shape.
    pub fn sso_assertion_desktop(
        &self,
        session: &Session,
        editor: bool,
        ret: &str,
        state: &str,
    ) -> Result<String, String> {
        if !self.sso_desktop_ready() {
            return Err("Sign-in is not set up yet.".into());
        }
        let ret = loopback_return(ret)
            .ok_or_else(|| "That return address is not a loopback address.".to_string())?;
        let iat = now();
        self.seal_with(
            &self.sso_secret,
            &SsoAssertionBody {
                aud: "desktop".into(),
                who: session.who.clone(),
                handle: session.handle.clone(),
                name: session.name.clone(),
                email: session.email.clone(),
                editor,
                ret,
                st: Some(state.to_string()),
                iat,
                exp: iat + SSO_TTL,
                jti: random_token(16)?,
            },
        )
    }

    /// Open an SSO assertion under the SSO secret — the exchange
    /// endpoint is the one place this server ever takes an assertion
    /// back, and only the desktop audience's. Expiry is checked
    /// here; audience, state, and single-use are the caller's to
    /// enforce (the burn lives on the `Gate`, which owns request
    /// state). Every failure is the same error: the exchange must
    /// not say which check a forged assertion failed.
    pub fn open_sso_assertion(&self, token: &str) -> Result<SsoAssertionBody, String> {
        let body = self.open_with::<SsoAssertionBody>(&self.sso_secret, token)?;
        if body.exp < now() {
            return Err("Sign-in did not complete.".into());
        }
        Ok(body)
    }

    fn open<T: for<'de> Deserialize<'de>>(&self, token: &str) -> Result<T, String> {
        self.open_with(&self.session_secret, token)
    }

    fn open_with<T: for<'de> Deserialize<'de>>(&self, secret: &str, token: &str) -> Result<T, String> {
        let (payload, sig) = token
            .split_once('.')
            .ok_or_else(|| "Sign-in did not complete.".to_string())?;
        if payload.len() > 4000 || sig.len() > 200 || !verify(secret, payload, sig) {
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

#[derive(Clone)]
enum Editor {
    Who(String),
    Email(String),
}

impl Editor {
    /// The line form used in the admitted-editors file: the account id
    /// (`github:100`) or the email, exactly as `Editors::parse` reads it.
    fn as_entry(&self) -> &str {
        match self {
            Editor::Who(id) => id,
            Editor::Email(email) => email,
        }
    }
}

/// Accounts allowed to use the editor once signed in.
///
/// The list starts from `PHEXT_EDITORS` (the operator's bootstrap list) and
/// can grow at runtime: when an editor accepts a submission, the
/// originator's account is admitted — appended here and, when a backing
/// file is configured (see `Gate::load_editors_file`), to that file, so
/// the admission takes effect immediately and survives restarts.
/// Entries sit behind a lock because admission happens through `&Gate`
/// while other requests are reading the list.
pub struct Editors {
    entries: std::sync::RwLock<Vec<Editor>>,
    file: std::sync::RwLock<Option<std::path::PathBuf>>,
}

impl Default for Editors {
    fn default() -> Self {
        Self { entries: std::sync::RwLock::new(Vec::new()), file: std::sync::RwLock::new(None) }
    }
}

/// One list entry from one raw token, or None when the token is unusable.
/// Shared by the comma-separated env form and the one-per-line file form.
fn parse_entry(part: &str) -> Option<Editor> {
    let part = part.trim();
    if part.is_empty() || part.len() > 120 {
        return None;
    }
    if part.contains('@') {
        let email = clean_email(part);
        if email.is_empty() {
            None
        } else {
            Some(Editor::Email(email))
        }
    } else if valid_who(part) {
        Some(Editor::Who(part.to_string()))
    } else {
        None
    }
}

impl Editors {
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("PHEXT_EDITORS").unwrap_or_default())
    }

    pub fn parse(raw: &str) -> Self {
        let editors = Self::default();
        *editors.entries.write().unwrap() =
            raw.split(',').filter_map(parse_entry).collect();
        editors
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().unwrap().is_empty()
    }

    pub fn matches(&self, who: &str, email: &str) -> bool {
        let email = email.trim().to_lowercase();
        self.entries.read().unwrap().iter().any(|entry| match entry {
            Editor::Who(id) => id == who,
            Editor::Email(expected) => !email.is_empty() && expected == &email,
        })
    }

    /// An empty list admits every signed-in account.
    pub fn allows(&self, who: &str, email: &str) -> bool {
        self.is_empty() || self.matches(who, email)
    }

    /// Point the list at its backing file and load any entries already in
    /// it (one per line; blank lines and `#` comments are ignored, and a
    /// line may still carry several comma-separated entries). Entries the
    /// list already has are not duplicated. Returns how many were loaded.
    pub fn load_file(&self, path: &std::path::Path) -> usize {
        *self.file.write().unwrap() = Some(path.to_path_buf());
        let Ok(text) = std::fs::read_to_string(path) else {
            return 0;
        };
        let mut loaded = 0;
        let mut entries = self.entries.write().unwrap();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            for part in line.split(',') {
                if let Some(entry) = parse_entry(part) {
                    if !entries.iter().any(|e| e.as_entry() == entry.as_entry()) {
                        entries.push(entry);
                        loaded += 1;
                    }
                }
            }
        }
        loaded
    }

    /// Admit an account: add its account id (when valid) and its email
    /// (when usable), unless each is already covered, and append the new
    /// entries to the backing file when one is configured. Takes effect
    /// immediately. Returns true when the account is an editor afterwards.
    pub fn admit(&self, who: &str, email: &str) -> bool {
        let mut fresh: Vec<Editor> = Vec::new();
        if valid_who(who) && !self.matches(who, "") {
            fresh.push(Editor::Who(who.to_string()));
        }
        let email = clean_email(email);
        if !email.is_empty() && !self.matches("", &email) {
            fresh.push(Editor::Email(email.clone()));
        }
        if !fresh.is_empty() {
            self.entries.write().unwrap().extend(fresh.iter().cloned());
            let path = self.file.read().unwrap().clone();
            if let Some(path) = path {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    use std::io::Write;
                    for entry in &fresh {
                        let _ = writeln!(out, "{}", entry.as_entry());
                    }
                }
            }
        }
        self.allows(who, &email)
    }
}

// ── Limiter ────────────────────────────────────────────────────────────

/// In-memory cap on sign-in attempts per client address, per ten minutes.
///
/// Counts OAuth start/callback attempts. Anonymous 401s are not counted,
/// so opening the signed-out page does not
/// spend the budget. The address is `X-Real-IP` when nginx sets it from
/// `$remote_addr`; otherwise the last `X-Forwarded-For` hop, which is the
/// one a single reverse proxy appends.
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
    secure_transport: bool,
}

impl Reply {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: value.to_string().into_bytes(),
            secure_transport: false,
        }
    }

    fn error(status: u16, message: &str) -> Self {
        let mut reply = Self::json(status, json!({ "ok": false, "error": message }));
        if status == 429 {
            reply.headers.push(("Retry-After", "600".into()));
        }
        reply
    }

    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            content_type: "text/plain",
            headers: vec![("Location", location)],
            body: Vec::new(),
            secure_transport: false,
        }
    }

    fn cookie(mut self, value: String) -> Self {
        self.headers.push(("Set-Cookie", value));
        self
    }

    /// Remember whether the browser reached us over HTTPS (via nginx).
    fn for_transport(mut self, headers: &HashMap<String, String>) -> Self {
        self.secure_transport = forwarded_https(headers);
        self
    }

    /// Serialize as an HTTP/1.1 response with `Connection: close`.
    ///
    /// Auth and session bodies stay `Cache-Control: no-store`. There is no
    /// `Access-Control-Allow-Origin`: the editor is same-origin only.
    pub fn write_to(&self, out: &mut impl std::io::Write) {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{}",
            self.status,
            status_text(self.status),
            self.content_type,
            self.body.len(),
            security_header_block(self.secure_transport)
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

/// One year. Matches `deploy/nginx-edit.phext.io.conf`. No
/// `includeSubDomains`: this host is `edit.phext.io`, and a future
/// sibling name under that host should not be pinned by accident.
pub const HSTS_VALUE: &str = "max-age=31536000";

/// Policy for the embedded editor and the public pages.
///
/// The shell is one HTML document: an inline script, an inline stylesheet,
/// `onclick` attributes, and `style` attributes. A nonce or a hash does not
/// cover event-handler or style attributes, so those would mean rewriting
/// the shell. `unsafe-inline` stays for that reason. There is no
/// `unsafe-eval`. Sign-in is a same-origin link to `/api/auth/...`, which
/// 302s to Google or GitHub; top-level navigations are not restricted.
/// `upgrade-insecure-requests` is added only when the browser connection is
/// HTTPS, so a local `http://127.0.0.1` session keeps working.
pub const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'self'; ",
    "script-src 'self' 'unsafe-inline'; ",
    "style-src 'self' 'unsafe-inline'; ",
    "img-src 'self'; ",
    "font-src 'self'; ",
    "connect-src 'self'; ",
    "form-action 'self'; ",
    "base-uri 'self'; ",
    "frame-ancestors 'none'; ",
    "object-src 'none'; ",
    "frame-src 'none'"
);

pub fn forwarded_https(headers: &HashMap<String, String>) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub fn security_header_block(https: bool) -> String {
    let mut csp = CONTENT_SECURITY_POLICY.to_string();
    if https {
        csp.push_str("; upgrade-insecure-requests");
    }
    let mut block = format!(
        "X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: strict-origin-when-cross-origin\r\n\
         Permissions-Policy: accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()\r\n\
         Content-Security-Policy: {csp}\r\n"
    );
    if https {
        block.push_str(&format!("Strict-Transport-Security: {HSTS_VALUE}\r\n"));
    }
    block
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

/// Access control for the editor servers: an OAuth session, or open when
/// sign-in is not configured.
pub struct Gate {
    oauth: Oauth,
    editors: Editors,
    limits: Limiter,
    /// CLI `--local` only. `enable_local` is the sole writer.
    local: AtomicBool,
    /// Claim mode (`/api/claim` writes). Off unless the operator turns
    /// it on: `PHEXT_CLAIM_ENABLED=1` via `from_env`, or `enable_claims`.
    claims: AtomicBool,
    /// Desktop SSO assertions already spent at the exchange endpoint:
    /// `jti` → its `exp`. Web audiences burn their own; the desktop
    /// audience cannot hold the secret, so its single-use is enforced
    /// here. Entries are pruned by expiry on every burn, so the set
    /// holds at most one assertion lifetime's worth of exchanges.
    burned: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

impl Gate {
    pub fn new(oauth: Oauth, editors: Editors) -> Self {
        Self {
            oauth,
            editors,
            limits: Limiter::standard(),
            local: AtomicBool::new(false),
            claims: AtomicBool::new(false),
            burned: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Spend a desktop assertion's `jti`. Returns false when it was
    /// already spent — a replay. Expired entries are dropped first,
    /// so the set cannot grow past the assertions one lifetime can
    /// hold.
    fn burn_jti(&self, jti: &str, exp: i64) -> bool {
        let mut burned = self.burned.lock().unwrap();
        let t = now();
        burned.retain(|_, e| *e >= t);
        if burned.contains_key(jti) {
            return false;
        }
        burned.insert(jti.to_string(), exp);
        true
    }

    pub fn from_env() -> Self {
        let gate = Self::new(Oauth::from_env(), Editors::from_env());
        if std::env::var("PHEXT_CLAIM_ENABLED").map(|v| v.trim() == "1").unwrap_or(false) {
            gate.enable_claims();
        }
        // Runtime admissions persist here when the operator names the file.
        // Without it, phext-edit wires in a default next to its phexts.
        if let Ok(path) = std::env::var("PHEXT_EDITORS_FILE") {
            let path = path.trim();
            if !path.is_empty() {
                gate.load_editors_file(std::path::Path::new(path));
            }
        }
        gate
    }

    /// Admit every direct request as a local developer.
    ///
    /// The phext-edit CLI calls this only after it has refused to bind
    /// anything but loopback. Nothing in the environment or in a request
    /// header calls it.
    pub fn enable_local(&self) {
        self.local.store(true, Ordering::SeqCst);
    }

    pub fn is_local(&self) -> bool {
        self.local.load(Ordering::SeqCst)
    }

    /// Open claim mode: `/api/claim` accepts claims (the lookup stays
    /// public either way). The operator turns this on once the X app
    /// credentials are in place; tests call it directly.
    pub fn enable_claims(&self) {
        self.claims.store(true, Ordering::SeqCst);
    }

    pub fn claims_open(&self) -> bool {
        self.claims.load(Ordering::SeqCst)
    }

    /// True when some credential is required for the API.
    pub fn locked(&self) -> bool {
        self.oauth.ready()
    }

    pub fn oauth(&self) -> &Oauth {
        &self.oauth
    }

    pub fn editors(&self) -> &Editors {
        &self.editors
    }

    /// The signed-in session for a request, if any. In `--local` mode this
    /// is the synthetic local developer (never for a forwarded request),
    /// mirroring `session_view`; otherwise it is the cookie session.
    pub fn session(&self, headers: &HashMap<String, String>) -> Option<Session> {
        if self.is_local() {
            if request_forwarded(headers) {
                return None;
            }
            return Some(Session {
                who: "local:developer".into(),
                handle: "local".into(),
                name: "Local developer".into(),
                email: String::new(),
                csrf: "local-dev".into(),
            });
        }
        self.oauth.read_session(&cookies(headers))
    }

    /// Whether a session belongs to an approved editor.
    pub fn is_editor(&self, session: &Session) -> bool {
        self.editors.allows(&session.who, &session.email)
    }

    /// Admit an account as an editor, effective immediately and persisted
    /// to the editors file when one is configured. Used when an editor
    /// accepts a submission: the originator joins the editors.
    pub fn admit_account(&self, who: &str, email: &str) -> bool {
        self.editors.admit(who, email)
    }

    /// Load additional editor entries from a file (one per line) and make
    /// it the backing file for future admissions. The phext-edit server
    /// calls this with its default admitted-editors file when
    /// `PHEXT_EDITORS_FILE` is not set.
    pub fn load_editors_file(&self, path: &std::path::Path) {
        self.editors.load_file(path);
    }

    /// Startup lines describing how the API is protected.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.is_local() {
            lines.push("auth: DISABLED (--local, local developer, direct requests only)".to_string());
            return lines;
        }
        if self.oauth.ready() {
            let who = if self.editors.is_empty() {
                "any signed-in account".to_string()
            } else {
                format!("{} listed editors", self.editors.len())
            };
            lines.push(format!("auth: sign-in via {} ({})", self.oauth.providers().join(", "), who));
        }
        if !self.locked() {
            lines.push("auth: open (sign-in not configured)".to_string());
        }
        if self.claims_open() {
            lines.push("claims: open (/api/claim accepts claims)".to_string());
        }
        lines
    }

    /// Handles `/api/session` and `/api/auth/*`. Returns None for other paths.
    pub fn route(&self, method: &str, path: &str, headers: &HashMap<String, String>) -> Option<Reply> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let reply = match (method, route) {
            ("GET", "/api/session") => self.session_view(headers),
            ("POST", "/api/auth/logout") => self.logout(headers),
            // The desktop exchange: the one assertion this server
            // ever takes back. Its credential rides in headers (the
            // route layer has no body channel; headers are how this
            // API already carries the CSRF credential). Before the
            // generic provider arm, which would read `desktop` as a
            // provider name.
            ("POST", "/api/auth/desktop/exchange") => self.desktop_exchange(headers),
            // Before the generic provider arm: without this arm, `sso`
            // would be parsed as a provider name below. The segment
            // after `sso/` is the audience; `Gate::sso` answers 404 for
            // anything outside the allowlist, so this one arm serves
            // every audience — `/api/auth/sso/mutable` behaves exactly
            // as it did when it stood alone here.
            ("GET", p) if p.starts_with("/api/auth/sso/") => {
                self.sso(&p["/api/auth/sso/".len()..], headers, &query_map(query))
            }
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
        Some(reply.for_transport(headers))
    }

    /// Decides whether a non-auth request may proceed.
    pub fn admit(&self, method: &str, path: &str, headers: &HashMap<String, String>) -> Result<(), Reply> {
        // `--local` is a process flag, not a header. Proxy markers withhold it.
        if self.is_local() {
            if request_forwarded(headers) {
                return Err(Reply::error(403, "local mode refuses a forwarded request"));
            }
            return Ok(());
        }
        let route = path.split('?').next().unwrap_or(path);
        if route == "/" || route == "/index.html" || !self.locked() {
            return Ok(());
        }
        if let Some(session) = self.oauth.read_session(&cookies(headers)) {
            if !self.editors.allows(&session.who, &session.email) {
                return Err(self.denied(headers, 403, "This account is not on the editor list."));
            }
            if method != "GET" && method != "HEAD" {
                let csrf = headers.get("x-csrf-token").map(String::as_str).unwrap_or("");
                if !self.oauth.csrf_matches(&session, csrf) {
                    return Err(self.denied(headers, 403, "That request did not match this sign-in."));
                }
            }
            return Ok(());
        }
        Err(self.denied(headers, 401, "unauthorized"))
    }

    fn denied(&self, headers: &HashMap<String, String>, status: u16, message: &str) -> Reply {
        Reply::error(status, message).for_transport(headers)
    }

    fn session_view(&self, headers: &HashMap<String, String>) -> Reply {
        if self.is_local() && !request_forwarded(headers) {
            return Reply::json(200, json!({
                "locked": true,
                "local": true,
                "ready": false,
                "providers": [],
                "signed_in": true,
                "editor": true,
                "who": "local:developer",
                "handle": "local",
                "name": "Local developer",
                "csrf": "local-dev",
            }));
        }
        let session = self.oauth.read_session(&cookies(headers));
        let editor = session
            .as_ref()
            .map(|s| self.editors.allows(&s.who, &s.email))
            .unwrap_or(false);
        let field = |f: fn(&Session) -> &str| session.as_ref().map(f).unwrap_or("").to_string();
        Reply::json(200, json!({
            "locked": self.locked(),
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

    /// Who may cross to an SSO audience: any signed-in account. The
    /// assertion still carries the account's real `editor` flag for
    /// the audience to display or use later. To make an audience
    /// editor-only, this one predicate becomes
    /// `self.is_editor(session)` — nothing else in the handoff changes.
    fn sso_admits(&self, _session: &Session) -> bool {
        true
    }

    /// `GET /api/auth/sso/<aud>` — the cross-site handoff to one
    /// allowlisted audience (`mutable`, `exocortexia`, `wishnode`).
    ///
    /// Layering follows the claims/submissions precedent in the servers:
    /// a signed-in account is enough; an editor's place is not required
    /// (`sso_admits` is the single predicate that decides). With a
    /// session, the reply 302s to the audience's callback with a freshly
    /// sealed assertion. Without one, it 302s to this site's own root
    /// with this endpoint as the `next`, so the ordinary provider
    /// sign-in returns the browser here and the handoff completes —
    /// the OAuth `next` itself stays same-site throughout. An audience
    /// outside the allowlist, or one not configured (no SSO secret, or
    /// no target for that audience), simply does not exist: 404 —
    /// checked before any session handling, so an unknown audience
    /// never enters the sign-in return flow.
    pub fn sso(
        &self,
        aud: &str,
        headers: &HashMap<String, String>,
        query: &HashMap<String, String>,
    ) -> Reply {
        if self.limits.hit(&client_key(headers)).is_err() {
            return Reply::error(429, "Too many requests.");
        }
        // The desktop audience has no configured target to look up —
        // its return is a per-attempt loopback URL — so it leaves the
        // shared path here, before the target check.
        if aud == "desktop" {
            return self.sso_desktop(headers, query);
        }
        let Some(target) = self.oauth.sso_target_for(aud) else {
            return Reply::error(404, "not found");
        };
        if !self.oauth.sso_ready_for(aud) {
            return Reply::error(404, "not found");
        }
        let ret = safe_next(query.get("return").map(String::as_str).unwrap_or("/"));
        let Some(session) = self.session(headers) else {
            let next = format!("/api/auth/sso/{aud}?return={}", form_encode(&ret));
            return Reply::redirect(format!("{}/?next={}", self.oauth.base(), form_encode(&next)));
        };
        if !self.sso_admits(&session) {
            return Reply::error(403, "This account is not on the editor list.");
        }
        let editor = self.is_editor(&session);
        match self.oauth.sso_assertion(aud, &session, editor, &ret) {
            Ok(assertion) => Reply::redirect(format!(
                "{target}/api/auth/callback?assertion={assertion}"
            )),
            Err(message) => Reply::error(403, &message),
        }
    }

    /// `GET /api/auth/sso/desktop` — the desktop audience's mint
    /// (Phext Notepad). Same shape as the web handoff (an unarmed
    /// audience 404s before any session handling; the signed-out are
    /// sent to sign-in with this endpoint as the `next`), with the
    /// two differences the audience exists for:
    ///
    /// * The return is not an operator-configured origin but the
    ///   app's own loopback listener, supplied per attempt as
    ///   `?return=` and admitted only through `loopback_return` —
    ///   an `http` URL on the `127.0.0.1` or `[::1]` literal with an
    ///   explicit port. Anything else (a hostname — `localhost`
    ///   included — another IP, `https`, a missing port) is a 400
    ///   before any session handling, so a bad return never enters
    ///   the sign-in bounce.
    /// * The app's per-attempt `state` nonce is required and rides
    ///   sealed inside the assertion; the exchange demands it back.
    ///
    /// The redirect carries the assertion and the state back to the
    /// listener as query parameters — the assertion is worthless
    /// almost immediately (120 seconds, single-use, burned at the
    /// exchange), the same posture as the web audiences' callbacks.
    fn sso_desktop(
        &self,
        headers: &HashMap<String, String>,
        query: &HashMap<String, String>,
    ) -> Reply {
        if !self.oauth.sso_desktop_ready() {
            return Reply::error(404, "not found");
        }
        let raw_ret = query.get("return").map(String::as_str).unwrap_or("");
        let Some(ret) = loopback_return(raw_ret) else {
            return Reply::error(400, "That return address is not a loopback address.");
        };
        let state = query.get("state").map(String::as_str).unwrap_or("");
        if !valid_state_nonce(state) {
            return Reply::error(400, "That sign-in request is missing its state.");
        }
        let Some(session) = self.session(headers) else {
            let next = format!(
                "/api/auth/sso/desktop?return={}&state={}",
                form_encode(&ret),
                form_encode(state)
            );
            return Reply::redirect(format!("{}/?next={}", self.oauth.base(), form_encode(&next)));
        };
        if !self.sso_admits(&session) {
            return Reply::error(403, "This account is not on the editor list.");
        }
        let editor = self.is_editor(&session);
        match self.oauth.sso_assertion_desktop(&session, editor, &ret, state) {
            Ok(assertion) => {
                let sep = if ret.contains('?') { '&' } else { '?' };
                Reply::redirect(format!(
                    "{ret}{sep}assertion={assertion}&state={}",
                    form_encode(state)
                ))
            }
            Err(message) => Reply::error(403, &message),
        }
    }

    /// `POST /api/auth/desktop/exchange` — the desktop audience's
    /// assertions come home. The app presents the assertion it caught
    /// on its loopback listener (`x-phext-assertion`) with the state
    /// of the attempt that minted it (`x-phext-state`); this server
    /// verifies the seal under the SSO secret (a shipped app cannot
    /// hold that secret, so verification lives here and nowhere
    /// else), the `desktop` audience, the expiry, and the state, and
    /// burns the `jti` — single-use is enforced on this side of the
    /// wire. Every validation failure is the same 401: the endpoint
    /// does not say which check a bad assertion failed.
    ///
    /// A verified assertion for an account on the editors list is
    /// answered with a freshly sealed session value and its CSRF
    /// token as JSON — the credential the app then presents exactly
    /// as if the user had handed it the cookie. The gate is checked
    /// fresh against the current list (not the assertion's `editor`
    /// claim): a signed-in non-editor's assertion still mints, but
    /// the exchange answers it with the gate's own refusal and no
    /// credential, and the attempt spends the assertion either way —
    /// once admitted, the app simply starts a new attempt.
    fn desktop_exchange(&self, headers: &HashMap<String, String>) -> Reply {
        if self.limits.hit(&client_key(headers)).is_err() {
            return Reply::error(429, "Too many requests.");
        }
        if !self.oauth.sso_desktop_ready() {
            return Reply::error(404, "not found");
        }
        let refuse = || Reply::error(401, "Sign-in did not complete.");
        let assertion = headers.get("x-phext-assertion").map(|v| v.trim()).unwrap_or("");
        let state = headers.get("x-phext-state").map(|v| v.trim()).unwrap_or("");
        if assertion.is_empty() || state.is_empty() {
            return refuse();
        }
        let Ok(body) = self.oauth.open_sso_assertion(assertion) else {
            return refuse();
        };
        if body.aud != "desktop" || body.st.as_deref() != Some(state) {
            return refuse();
        }
        if !valid_who(&body.who) || !self.burn_jti(&body.jti, body.exp) {
            return refuse();
        }
        if !self.editors.allows(&body.who, &body.email) {
            return Reply::error(403, "This account is not on the editor list.");
        }
        match self.oauth.issue_session(&body.who, &body.handle, &body.name, &body.email) {
            Ok((session, csrf)) => Reply::json(200, json!({ "session": session, "csrf": csrf })),
            Err(_) => Reply::error(500, "Sign-in did not complete."),
        }
    }
}

fn auth_failed(secure: bool) -> Reply {
    Reply {
        status: 400,
        content_type: "text/html; charset=utf-8",
        headers: Vec::new(),
        body: b"<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>phext</title></head><body><p>Sign-in did not complete. <a href=\"./\">Back</a></p></body></html>".to_vec(),
        secure_transport: false,
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
    if let Some(ip) = headers.get("x-real-ip").and_then(|value| clean_ip(value)) {
        return ip;
    }
    if let Some(value) = headers.get("x-forwarded-for") {
        if let Some(ip) = value.split(',').rev().find_map(clean_ip) {
            return ip;
        }
    }
    "local".into()
}

fn clean_ip(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':'))
    {
        return None;
    }
    Some(value.to_string())
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
///
/// A `:` is refused in the path portion only. Past the `?`, a query may
/// legitimately carry URLs of its own — an OAuth authorization request
/// returning from sign-in names https redirect URIs and, for CIMD
/// clients, an https client_id — and a colon there cannot move the
/// destination off this site, because the value still begins with a
/// single `/` and browsers resolve it as a path on this origin. The
/// length cap admits those same authorization requests (a full
/// `/oauth/authorize?...` round trip runs several hundred characters);
/// the sealed pending cookie that carries `next` through a provider
/// sign-in holds it comfortably.
pub fn safe_next(next: &str) -> String {
    let next = next.trim();
    let path_end = next.find(['?', '#']).unwrap_or(next.len());
    if next.len() > 2048
        || !next.starts_with('/')
        || next.starts_with("//")
        || next.contains('\\')
        || next[..path_end].contains(':')
        || next.chars().any(|ch| ch.is_control() || ch == ' ')
    {
        return "/".into();
    }
    next.to_string()
}

/// Admit a desktop return URL: the address of the listener the app
/// opened for this one sign-in attempt. The bar is the loopback
/// literal itself, checked character by character — not a URL parse
/// that a hostname or userinfo could slip through:
///
/// * scheme `http` exactly. Loopback has a real origin but no CA
///   issues certificates for it; RFC 8252's native-app practice is
///   plain http on the loopback interface, which never leaves the
///   machine.
/// * host the literal `127.0.0.1` or `[::1]` — not `localhost`, not
///   any other name, not another loopback address. A name is
///   whatever the resolver says it is (hosts files are editable),
///   and the app binds the numeric loopback anyway.
/// * an explicit port, 1–65535 — the app always listens on an
///   ephemeral port it was given, never a default.
/// * then only a path/query tail of plain URL characters: no `#`
///   (a fragment never reaches a server anyway, and its presence
///   means the value was not a listener address), no whitespace,
///   controls, or non-ASCII.
///
/// The result is capped short enough to ride the sign-in bounce —
/// this endpoint travels as the `next` of a `/?next=` redirect, well
/// inside the length `safe_next` admits there.
pub fn loopback_return(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.len() > 160
        || raw.contains('#')
        || raw.contains('\\')
        || raw.chars().any(|ch| ch.is_control() || ch == ' ' || !ch.is_ascii())
    {
        return None;
    }
    let rest = raw.strip_prefix("http://")?;
    let authority_len = rest
        .find(|ch| ch == '/' || ch == '?')
        .unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_len);
    let (host, port) = if let Some(v6) = authority.strip_prefix("[::1]:") {
        ("[::1]", v6)
    } else if let Some(v4) = authority.strip_prefix("127.0.0.1:") {
        ("127.0.0.1", v4)
    } else {
        return None;
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u32 = port.parse().ok()?;
    if port == 0 || port > 65535 {
        return None;
    }
    if !tail
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-._~:/?&=%+@!$'()*,".contains(ch))
    {
        return None;
    }
    Some(format!("http://{host}:{port}{tail}"))
}

/// The desktop attempt's state nonce: long enough to be unguessable,
/// plain enough to travel in URLs and assertions unchanged. The app
/// generates it (32 random hex/letters in practice); this only
/// checks the shape.
pub fn valid_state_nonce(state: &str) -> bool {
    (16..=128).contains(&state.len())
        && state
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
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
            // X user ids are numeric strings, like GitHub's.
            "x" => id.chars().all(|ch| ch.is_ascii_digit()),
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

/// An X username verbatim: `[A-Za-z0-9_]{1,15}`, case preserved. Unlike
/// `clean_handle` this keeps underscores — they are load-bearing in X
/// handles (@Grady_Booch, @4ster_light) and the wavefront lookup keys on
/// them. Anything outside the charset makes the handle unusable ("").
pub fn clean_twitter_handle(raw: &str) -> String {
    let handle = raw.trim().strip_prefix('@').unwrap_or(raw.trim());
    if handle.is_empty()
        || handle.len() > 15
        || !handle.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return String::new();
    }
    handle.to_string()
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

/// X (Twitter) OAuth 2.0: a confidential client, so the token exchange
/// carries HTTP Basic (client id + secret) alongside the PKCE form body.
/// `GET /2/users/me` returns the account's numeric id and its `username`
/// — the username is the identity the wavefront map is keyed by, so it is
/// kept verbatim (case and underscores) via `clean_twitter_handle`.
/// X returns no email under `tweet.read users.read`; email stays empty,
/// which simply means X accounts match allowlists by `x:<id>` only.
fn x_identity(
    agent: &ureq::Agent,
    code: &str,
    verifier: &str,
    redirect: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<Identity, String> {
    let form = format!(
        "code={}&grant_type=authorization_code&client_id={}&redirect_uri={}&code_verifier={}",
        form_encode(code),
        form_encode(client_id),
        form_encode(redirect),
        form_encode(verifier),
    );
    let basic = base64_std(format!("{}:{}", client_id, client_secret).as_bytes());
    let token = agent
        .post("https://api.twitter.com/2/oauth2/token")
        .set("Accept", "application/json")
        .set("User-Agent", "phext-lattice")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Authorization", &format!("Basic {basic}"))
        .send_string(&form)
        .map_err(|_| "the provider did not complete sign-in.")?
        .into_string()
        .map_err(|_| "the provider did not complete sign-in.")?;
    let access = access_token(&token)?;
    let user = get_json(agent, "https://api.twitter.com/2/users/me", "application/json", &access)?;
    let data = user.get("data").unwrap_or(&user);
    let id = data
        .get("id")
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
        .unwrap_or_default();
    if !valid_who(&format!("x:{id}")) {
        return Err("the provider did not complete sign-in.".into());
    }
    let handle = clean_twitter_handle(data.get("username").and_then(|v| v.as_str()).unwrap_or(""));
    if handle.is_empty() {
        return Err("the provider did not complete sign-in.".into());
    }
    let profile = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
    Ok(Identity {
        who: format!("x:{id}"),
        handle: handle.clone(),
        name: display_name(profile, &handle),
        email: String::new(),
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

/// Standard base64 (with `+`, `/`, and `=` padding) — HTTP Basic wants
/// this alphabet, not the URL-safe one used for cookies and PKCE.
fn base64_std(bytes: &[u8]) -> String {
    const STD: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(STD[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

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

/// True when `host` is a loopback listen address: 127.0.0.0/8, `::1`, or `localhost`.
///
/// `0.0.0.0` and `::` are not loopback — they accept remote connections.
pub fn is_loopback_bind(host: &str) -> bool {
    host_is_loopback(host)
}

fn host_is_loopback(host: &str) -> bool {
    let host = host.trim().trim_matches('"');
    if host.is_empty() {
        return false;
    }
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else if host.matches(':').count() > 1 {
        host
    } else {
        host.split(':').next().unwrap_or("")
    };
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    name == "localhost" || name == "::1" || name == "0:0:0:0:0:0:0:1" || ipv4_loopback(&name)
}

fn ipv4_loopback(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let Ok(first) = parts[0].parse::<u8>() else {
        return false;
    };
    first == 127
        && parts[1].parse::<u8>().is_ok()
        && parts[2].parse::<u8>().is_ok()
        && parts[3].parse::<u8>().is_ok()
}

/// Proxy markers, or a Host that is not loopback.
///
/// Presence withholds `--local`. These headers never grant it.
fn request_forwarded(headers: &HashMap<String, String>) -> bool {
    const PROXY: &[&str] = &[
        "x-forwarded-for",
        "x-real-ip",
        "x-forwarded-host",
        "x-forwarded-proto",
        "forwarded",
    ];
    for name in PROXY {
        if headers.get(*name).map(|value| !value.trim().is_empty()).unwrap_or(false) {
            return true;
        }
    }
    match headers.get("host") {
        Some(host) if !host_is_loopback(host) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn gate(editors: &str) -> Gate {
        Gate::new(Oauth::test_ready(), Editors::parse(editors))
    }

    fn header<'a>(reply: &'a Reply, name: &str) -> Vec<&'a str> {
        reply.headers.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect()
    }

    #[test]
    fn mcp_oauth_hash_is_domain_separated_and_keyed() {
        let oauth = Oauth::test_ready();
        let code = oauth.mcp_oauth_hash("authorization-code", "secret-value").unwrap();
        let token = oauth.mcp_oauth_hash("access-token", "secret-value").unwrap();
        let other = oauth.mcp_oauth_hash("authorization-code", "other-value").unwrap();
        assert_ne!(code, token, "domains must separate credential kinds");
        assert_ne!(code, other, "values must change the digest");
        assert_eq!(code.len(), 64, "SHA-256 hex");
        assert!(!code.contains("secret-value"), "the digest never carries the raw value");
        assert!(oauth.mcp_oauth_hash("", "x").is_none());
        assert!(oauth.mcp_oauth_hash("access-token", "").is_none());
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
        // A colon in the path portion is still refused, query or not.
        assert_eq!(safe_next("/foo:bar?next=https://evil.example"), "/");
    }

    #[test]
    fn oauth_authorize_next_survives_sign_in() {
        // The MCP authorization endpoint bounces a signed-out visitor to
        // sign-in with the whole authorize request as `next`. Its query
        // carries https URLs (the redirect URI, the resource, a CIMD
        // client_id) and runs well past the old 200-character cap: both
        // used to clamp the return to `/`, stranding the flow on the
        // homepage. The full request must come through verbatim.
        let authorize = "/oauth/authorize?response_type=code&client_id=https%3A%2F%2Fclient.example%2Fclient.json&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&code_challenge=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~&code_challenge_method=S256&scope=phext%3Aread&resource=https%3A%2F%2Fedit.phext.io%2Fmcp&state=opaque-state";
        assert!(authorize.len() > 200);
        assert_eq!(safe_next(authorize), authorize);
        // The decoded form the shell hands back after one decode pass
        // carries literal colons in the query; those are admitted too.
        let decoded = "/oauth/authorize?response_type=code&client_id=https://client.example/client.json&redirect_uri=https://client.example/callback&resource=https://edit.phext.io/mcp";
        assert_eq!(safe_next(decoded), decoded);
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
    fn x_is_a_provider_with_a_twitter_authorize_url() {
        let oauth = Oauth::test_ready();
        assert_eq!(oauth.providers(), vec!["github", "google", "x"]);
        let (url, _pending) = oauth.authorize_url("x", "/claim").unwrap();
        assert!(url.starts_with("https://twitter.com/i/oauth2/authorize?"), "{url}");
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("client_id=x-client-id"), "{url}");
        assert!(url.contains("scope=tweet.read%20users.read"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fapi%2Fauth%2Fx%2Fcallback"), "{url}");
    }

    #[test]
    fn base64_std_matches_the_rfc_vectors() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"foob"), "Zm9vYg==");
        assert_eq!(base64_std(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn x_account_ids_and_handles_validate() {
        assert!(valid_who("x:1001"));
        assert!(!valid_who("x:1001a"));
        assert!(!valid_who("x:"));
        // Underscores survive; case survives; junk does not.
        assert_eq!(clean_twitter_handle("Grady_Booch"), "Grady_Booch");
        assert_eq!(clean_twitter_handle("@4ster_light"), "4ster_light");
        assert_eq!(clean_twitter_handle("__apf__"), "__apf__");
        assert_eq!(clean_twitter_handle("has space"), "");
        assert_eq!(clean_twitter_handle("dash-not-allowed"), "");
        assert_eq!(clean_twitter_handle("sixteen_chars_16"), "");
        // The generic cleaner would have eaten those underscores.
        assert_eq!(clean_handle("Grady_Booch"), "GradyBooch");
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
    fn admit_account_is_live_and_persists_to_the_editors_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("admitted-editors.txt");
        let gate = gate("github:1");
        gate.load_editors_file(&file);
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        // github:100 starts outside the list: the gate turns it away.
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);

        assert!(gate.admit_account("github:100", "Octo@Example.com"));
        // Live immediately: the same session now passes the gate.
        assert!(gate.admit("GET", "/api/nav", &headers(&[("cookie", &cookie)])).is_ok());
        assert!(gate.editors().matches("", "octo@example.com"));
        let view = gate.route("GET", "/api/session", &headers(&[("cookie", &cookie)])).unwrap();
        assert!(String::from_utf8_lossy(&view.body).contains("\"editor\":true"));

        // Persisted: one entry per line, and a fresh list loaded from the
        // same file (the restart case) sees both entries.
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().collect::<Vec<_>>(), ["github:100", "octo@example.com"]);
        let reloaded = Editors::parse("github:1");
        assert_eq!(reloaded.load_file(&file), 2);
        assert!(reloaded.allows("github:100", ""));
        assert!(reloaded.allows("google:sub-9", "octo@example.com"));

        // Admitting again adds nothing and writes nothing new.
        assert!(gate.admit_account("github:100", "octo@example.com"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    }

    #[test]
    fn an_open_gate_admits_everything() {
        let gate = Gate::new(Oauth::closed(), Editors::default());
        assert!(!gate.locked());
        assert!(gate.admit("POST", "/api/update", &headers(&[])).is_ok());
    }

    #[test]
    fn a_bearer_header_no_longer_admits() {
        // The old static-token auth predated sign-in and was removed:
        // an Authorization header, right or wrong, admits nobody now.
        let gate = Gate::new(Oauth::test_ready(), Editors::default());
        assert!(gate.admit("GET", "/", &headers(&[])).is_ok());
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[])).unwrap_err().status, 401);
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[("authorization", "Bearer s3cret-token")])).unwrap_err().status, 401);
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[("authorization", "Bearer nope")])).unwrap_err().status, 401);
    }

    #[test]
    fn a_session_needs_the_editor_list_and_csrf_for_writes() {
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        let outsider = gate("github:999");
        assert_eq!(outsider.admit("GET", "/api/nav", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);

        let gate = gate("github:100");
        assert!(gate.admit("GET", "/api/zoom/y/1.1.1?x=1", &headers(&[("cookie", &cookie)])).is_ok());
        assert_eq!(gate.admit("POST", "/api/update", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);
        assert!(gate
            .admit("POST", "/api/update", &headers(&[("cookie", &cookie), ("x-csrf-token", TEST_CSRF)]))
            .is_ok());
    }

    #[test]
    fn an_empty_editor_list_admits_any_signed_in_account() {
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        let gate = gate("");
        assert_eq!(gate.admit("GET", "/api/nav", &headers(&[])).unwrap_err().status, 401);
        assert!(gate.admit("GET", "/api/nav", &headers(&[("cookie", &cookie)])).is_ok());
        assert!(gate
            .admit("POST", "/api/update", &headers(&[("cookie", &cookie), ("x-csrf-token", TEST_CSRF)]))
            .is_ok());
        assert_eq!(gate.admit("POST", "/api/update", &headers(&[("cookie", &cookie)])).unwrap_err().status, 403);
    }

    #[test]
    fn the_full_sign_in_flow_sets_a_session() {
        let gate = gate("github:100");
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
    fn sign_in_returns_to_an_oauth_authorize_request() {
        // The MCP authorize endpoint sends a signed-out visitor to
        // sign-in with the whole authorize request as `next`. The flow
        // must land back on that exact request — query, https URLs and
        // all — not on the homepage.
        let gate = gate("github:100");
        let next = "/oauth/authorize?response_type=code&client_id=https://client.example/client.json&redirect_uri=https://client.example/callback&code_challenge=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789&code_challenge_method=S256&scope=phext:read&resource=https://edit.phext.io/mcp&state=opaque";
        let start = gate
            .route("GET", &format!("/api/auth/github?next={}", form_encode(next)), &headers(&[]))
            .unwrap();
        assert_eq!(start.status, 302);
        let location = header(&start, "Location")[0].to_string();
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let pending = header(&start, "Set-Cookie")[0].split(';').next().unwrap().to_string();
        let back = gate
            .route("GET", &format!("/api/auth/github/callback?code=ok&state={state}"), &headers(&[("cookie", &pending)]))
            .unwrap();
        assert_eq!(back.status, 302);
        assert_eq!(header(&back, "Location")[0], format!("http://127.0.0.1:8080{next}"));
    }

    #[test]
    fn logout_needs_csrf_and_clears_the_cookie() {
        let gate = gate("github:100");
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        assert_eq!(gate.route("POST", "/api/auth/logout", &headers(&[("cookie", &cookie)])).unwrap().status, 403);
        let out = gate
            .route("POST", "/api/auth/logout", &headers(&[("cookie", &cookie), ("x-csrf-token", TEST_CSRF)]))
            .unwrap();
        assert_eq!(out.status, 200);
        assert!(header(&out, "Set-Cookie")[0].contains("Max-Age=0"));
    }

    /// Drives a scripted provider flow through `Gate::route` and returns
    /// the `phext_session=…` cookie pair it sets.
    fn scripted_session(gate: &Gate, provider: &str, next: &str) -> String {
        let start = gate.route("GET", &format!("/api/auth/{provider}?next={next}"), &headers(&[])).unwrap();
        assert_eq!(start.status, 302);
        let location = header(&start, "Location")[0].to_string();
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let pending = header(&start, "Set-Cookie")[0].split(';').next().unwrap().to_string();
        let back = gate
            .route("GET", &format!("/api/auth/{provider}/callback?code=ok&state={state}"), &headers(&[("cookie", &pending)]))
            .unwrap();
        assert_eq!(back.status, 302);
        header(&back, "Set-Cookie")[0].split(';').next().unwrap().to_string()
    }

    /// Opens a sealed assertion with an explicit secret, the way
    /// Mutable's callback will — and only with the SSO secret.
    fn open_assertion(secret: &str, token: &str) -> serde_json::Value {
        let (payload, sig) = token.split_once('.').unwrap();
        assert!(verify(secret, payload, sig), "assertion must verify under the SSO secret");
        serde_json::from_slice(&base64url_decode(payload).unwrap()).unwrap()
    }

    #[test]
    fn sso_sends_the_signed_out_to_sign_in_with_itself_as_next() {
        // The `next` nests: the shell decodes it once to build the
        // provider link, and the endpoint decodes the `return` inside
        // it when the browser comes back — so assertions decode twice
        // to see the path the endpoint will act on.
        let gate = gate("");
        let reply = gate.route("GET", "/api/auth/sso/mutable", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
        let location = header(&reply, "Location")[0];
        let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
        assert_eq!(percent_decode(&percent_decode(next)), "/api/auth/sso/mutable?return=/");

        // A specific return path rides along, still same-site.
        let reply = gate.route("GET", "/api/auth/sso/mutable?return=/superposition", &headers(&[])).unwrap();
        let location = header(&reply, "Location")[0];
        let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
        assert_eq!(percent_decode(&percent_decode(next)), "/api/auth/sso/mutable?return=/superposition");

        // Hostile returns are clamped by safe_next before they are
        // carried anywhere: the sealed-and-returned path becomes `/`.
        for hostile in [
            "/api/auth/sso/mutable?return=https%3A%2F%2Fevil.example%2Fx",
            "/api/auth/sso/mutable?return=%2F%2Fevil.example",
            "/api/auth/sso/mutable?return=%2F%5Cevil",
        ] {
            let reply = gate.route("GET", hostile, &headers(&[])).unwrap();
            assert_eq!(reply.status, 302);
            let location = header(&reply, "Location")[0];
            let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
            assert_eq!(percent_decode(&percent_decode(next)), "/api/auth/sso/mutable?return=/", "{hostile}");
        }
    }

    #[test]
    fn sso_mints_a_short_lived_assertion_for_a_signed_in_account() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/mutable");
        assert!(session.starts_with("phext_session="));

        let jump = gate
            .route("GET", "/api/auth/sso/mutable?return=/superposition", &headers(&[("cookie", &session)]))
            .unwrap();
        assert_eq!(jump.status, 302);
        let location = header(&jump, "Location")[0];
        let assertion = location
            .strip_prefix("https://mutable.phext.io/api/auth/callback?assertion=")
            .unwrap();
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", assertion);
        assert_eq!(body["aud"], "mutable");
        assert_eq!(body["who"], "google:sub-1");
        assert_eq!(body["handle"], "ada");
        assert_eq!(body["name"], "Ada Example");
        assert_eq!(body["email"], "ada@example.com");
        // The editors list is empty: any signed-in account is an editor.
        assert_eq!(body["editor"], true);
        assert_eq!(body["ret"], "/superposition");
        assert_eq!(body["exp"].as_i64().unwrap() - body["iat"].as_i64().unwrap(), 120);
        assert!(!body["jti"].as_str().unwrap().is_empty());

        // The session secret does not open it: the keys never substitute.
        let (payload, sig) = assertion.split_once('.').unwrap();
        assert!(!verify("test-session-secret-must-be-32chars", payload, sig));
    }

    #[test]
    fn sso_carries_editor_false_for_a_signed_in_non_editor() {
        // The list names someone else: the account still crosses (any
        // signed-in account may), but the flag tells the truth.
        let gate = gate("github:999");
        let session = scripted_session(&gate, "google", "/api/auth/sso/mutable");
        let jump = gate.route("GET", "/api/auth/sso/mutable", &headers(&[("cookie", &session)])).unwrap();
        assert_eq!(jump.status, 302);
        let location = header(&jump, "Location")[0];
        let assertion = location
            .strip_prefix("https://mutable.phext.io/api/auth/callback?assertion=")
            .unwrap();
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", assertion);
        assert_eq!(body["who"], "google:sub-1");
        assert_eq!(body["editor"], false);
        assert_eq!(body["ret"], "/");
    }

    #[test]
    fn sso_is_404_until_configured_and_never_swallows_a_provider() {
        let closed_gate = Gate::new(Oauth::closed(), Editors::default());
        assert!(!closed_gate.oauth().sso_ready());
        let reply = closed_gate.route("GET", "/api/auth/sso/mutable", &headers(&[])).unwrap();
        assert_eq!(reply.status, 404);
        // Arm ordering: a real provider start still reaches the generic
        // arm (403 here only because the closed gate has no sign-in).
        let provider = closed_gate.route("GET", "/api/auth/github", &headers(&[])).unwrap();
        assert_eq!(provider.status, 403);

        // Arm precision: `/api/auth/sso` without `/mutable` is not the
        // handoff — it parses as a provider name and is refused as one.
        let ready_gate = gate("");
        let reply = ready_gate.route("GET", "/api/auth/sso", &headers(&[])).unwrap();
        assert_eq!(reply.status, 403);
    }

    #[test]
    fn sso_exocortexia_mints_an_assertion_with_its_own_audience() {
        // The second allowlisted audience mints through the same route
        // shape, under the same SSO secret, in the same format — the
        // only differences are the `aud` claim and the callback origin.
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/exocortexia");
        let jump = gate
            .route(
                "GET",
                "/api/auth/sso/exocortexia?return=/contracts",
                &headers(&[("cookie", &session)]),
            )
            .unwrap();
        assert_eq!(jump.status, 302);
        let location = header(&jump, "Location")[0];
        let assertion = location
            .strip_prefix("https://exocortexia.example/api/auth/callback?assertion=")
            .unwrap();
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", assertion);
        assert_eq!(body["aud"], "exocortexia");
        assert_eq!(body["who"], "google:sub-1");
        assert_eq!(body["handle"], "ada");
        assert_eq!(body["editor"], true);
        assert_eq!(body["ret"], "/contracts");
        assert_eq!(body["exp"].as_i64().unwrap() - body["iat"].as_i64().unwrap(), 120);
        assert!(!body["jti"].as_str().unwrap().is_empty());

        // The session secret does not open it: the keys never substitute.
        let (payload, sig) = assertion.split_once('.').unwrap();
        assert!(!verify("test-session-secret-must-be-32chars", payload, sig));
    }

    #[test]
    fn sso_exocortexia_sends_the_signed_out_to_sign_in_with_itself_as_next() {
        let gate = gate("");
        let reply = gate.route("GET", "/api/auth/sso/exocortexia", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
        let location = header(&reply, "Location")[0];
        let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
        assert_eq!(
            percent_decode(&percent_decode(next)),
            "/api/auth/sso/exocortexia?return=/"
        );
    }

    #[test]
    fn sso_rejects_an_audience_outside_the_allowlist() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/mutable");
        for path in ["/api/auth/sso/evil", "/api/auth/sso/mutable2", "/api/auth/sso/Mutable"] {
            // Signed in or out, an unknown audience is a plain 404 —
            // never a sign-in redirect that carries it as the `next`,
            // and never an assertion.
            let signed_in = gate.route("GET", path, &headers(&[("cookie", &session)])).unwrap();
            assert_eq!(signed_in.status, 404, "{path}");
            let signed_out = gate.route("GET", path, &headers(&[])).unwrap();
            assert_eq!(signed_out.status, 404, "{path}");
        }
    }

    #[test]
    fn sso_exocortexia_is_404_until_its_url_is_configured() {
        // The shared secret is set and Mutable is configured (its
        // default target), but no Exocortexia URL: that endpoint does
        // not exist, while Mutable's works exactly as before.
        let oauth = Oauth::from_parts(
            "http://127.0.0.1:8080",
            "test-session-secret-must-be-32chars",
            true,
            "github-client-id",
            "github-client-secret",
            "google-client-id",
            "google-client-secret",
            "x-client-id",
            "x-client-secret",
            "test-sso-secret-must-be-32-chars-minimum",
            "",
            "",
            "",
            false,
            Arc::new(ScriptedSource),
        );
        assert!(oauth.sso_ready());
        assert!(!oauth.sso_ready_for("exocortexia"));
        let gate = Gate::new(oauth, Editors::parse(""));
        let reply = gate.route("GET", "/api/auth/sso/exocortexia", &headers(&[])).unwrap();
        assert_eq!(reply.status, 404);
        let reply = gate.route("GET", "/api/auth/sso/mutable", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
    }


    #[test]
    fn sso_wishnode_mints_an_assertion_with_its_own_audience() {
        // The third allowlisted audience mints through the same route
        // shape, under the same SSO secret, in the same format — the
        // only differences are the `aud` claim and the callback origin.
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/wishnode");
        let jump = gate
            .route(
                "GET",
                "/api/auth/sso/wishnode?return=/intake",
                &headers(&[("cookie", &session)]),
            )
            .unwrap();
        assert_eq!(jump.status, 302);
        let location = header(&jump, "Location")[0];
        let assertion = location
            .strip_prefix("https://wishnode.example/api/auth/callback?assertion=")
            .unwrap();
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", assertion);
        assert_eq!(body["aud"], "wishnode");
        assert_eq!(body["who"], "google:sub-1");
        assert_eq!(body["handle"], "ada");
        assert_eq!(body["editor"], true);
        assert_eq!(body["ret"], "/intake");
        assert_eq!(body["exp"].as_i64().unwrap() - body["iat"].as_i64().unwrap(), 120);
        assert!(!body["jti"].as_str().unwrap().is_empty());

        // The session secret does not open it: the keys never substitute.
        let (payload, sig) = assertion.split_once('.').unwrap();
        assert!(!verify("test-session-secret-must-be-32chars", payload, sig));
    }

    #[test]
    fn sso_wishnode_sends_the_signed_out_to_sign_in_with_itself_as_next() {
        let gate = gate("");
        let reply = gate.route("GET", "/api/auth/sso/wishnode", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
        let location = header(&reply, "Location")[0];
        let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
        assert_eq!(
            percent_decode(&percent_decode(next)),
            "/api/auth/sso/wishnode?return=/"
        );
    }

    #[test]
    fn sso_wishnode_is_404_until_its_url_is_configured() {
        // The shared secret is set and both earlier audiences are
        // configured, but no Wishnode URL: that endpoint does not
        // exist, while Mutable's and Exocortexia's work as before.
        let oauth = Oauth::from_parts(
            "http://127.0.0.1:8080",
            "test-session-secret-must-be-32chars",
            true,
            "github-client-id",
            "github-client-secret",
            "google-client-id",
            "google-client-secret",
            "x-client-id",
            "x-client-secret",
            "test-sso-secret-must-be-32-chars-minimum",
            "",
            "https://exocortexia.example",
            "",
            false,
            Arc::new(ScriptedSource),
        );
        assert!(oauth.sso_ready());
        assert!(oauth.sso_ready_for("exocortexia"));
        assert!(!oauth.sso_ready_for("wishnode"));
        let gate = Gate::new(oauth, Editors::parse(""));
        let reply = gate.route("GET", "/api/auth/sso/wishnode", &headers(&[])).unwrap();
        assert_eq!(reply.status, 404);
        let reply = gate.route("GET", "/api/auth/sso/exocortexia", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
        let reply = gate.route("GET", "/api/auth/sso/mutable", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
    }

    #[test]
    fn sso_consumer_rejects_an_assertion_minted_for_another_audience() {
        // The consumer-side half of the `aud` binding: an assertion is
        // only good at the audience named in its sealed `aud` claim.
        // A token minted for Exocortexia, opened under the shared
        // secret exactly as a consumer opens it, names the wrong
        // audience for Wishnode and must be refused there.
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/exocortexia");
        let jump = gate
            .route("GET", "/api/auth/sso/exocortexia", &headers(&[("cookie", &session)]))
            .unwrap();
        let location = header(&jump, "Location")[0];
        let assertion = location
            .strip_prefix("https://exocortexia.example/api/auth/callback?assertion=")
            .unwrap();
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", assertion);
        assert_eq!(body["aud"], "exocortexia");
        assert_ne!(body["aud"], "wishnode");
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
    fn loopback_bind_is_only_loopback() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("127.0.0.2"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("[::1]"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("LOCALHOST"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("::"));
        assert!(!is_loopback_bind("[::]"));
        assert!(!is_loopback_bind("192.168.0.10"));
        assert!(!is_loopback_bind("edit.phext.io"));
        assert!(!is_loopback_bind(""));
    }

    #[test]
    fn local_mode_admits_direct_requests_as_a_local_developer() {
        let gate = Gate::new(Oauth::closed(), Editors::default());
        gate.enable_local();
        assert!(gate.is_local());
        assert!(gate.admit("GET", "/api/status", &headers(&[("host", "127.0.0.1:8080")])).is_ok());
        assert!(gate.admit("POST", "/api/update", &headers(&[("host", "localhost:8080")])).is_ok());
        assert!(gate.admit("GET", "/api/nav", &headers(&[("host", "[::1]:8080")])).is_ok());
        let session = gate.route("GET", "/api/session", &headers(&[("host", "127.0.0.1:9")])).unwrap();
        assert_eq!(session.status, 200);
        let body = String::from_utf8(session.body).unwrap();
        assert!(body.contains("\"signed_in\":true"));
        assert!(body.contains("\"editor\":true"));
        assert!(body.contains("local:developer"));
    }

    #[test]
    fn local_mode_admits_requests_carrying_an_authorization_header() {
        // Since the token auth was removed the header admits nobody by
        // itself; local mode admits direct requests regardless of it,
        // and no rate limit applies to the repetitions.
        let gate = Gate::new(Oauth::closed(), Editors::default());
        gate.enable_local();
        let h = headers(&[("host", "127.0.0.1:8080"), ("authorization", "Bearer nope")]);
        for _ in 0..(AUTH_PER_IP + 5) {
            assert!(gate.admit("GET", "/api/nav", &h).is_ok());
        }
    }

    #[test]
    fn local_mode_refuses_a_forwarded_request() {
        let gate = Gate::new(Oauth::closed(), Editors::default());
        gate.enable_local();
        let cases = [
            headers(&[("host", "127.0.0.1"), ("x-forwarded-for", "203.0.113.4")]),
            headers(&[("host", "127.0.0.1"), ("x-real-ip", "203.0.113.4")]),
            headers(&[("host", "127.0.0.1"), ("x-forwarded-proto", "https")]),
            headers(&[("host", "127.0.0.1"), ("forwarded", "for=203.0.113.4")]),
            headers(&[("x-forwarded-host", "edit.phext.io"), ("host", "127.0.0.1")]),
            headers(&[("host", "edit.phext.io")]),
        ];
        for headers in cases {
            let reply = gate.admit("GET", "/api/status", &headers).unwrap_err();
            assert_eq!(reply.status, 403);
        }
        let session = gate
            .route("GET", "/api/session", &headers(&[("host", "edit.phext.io")]))
            .unwrap();
        let body = String::from_utf8(session.body).unwrap();
        assert!(!body.contains("local:developer"));
        assert!(body.contains("\"signed_in\":false"));
    }

    #[test]
    fn local_mode_cannot_be_enabled_by_the_environment_or_a_header() {
        let env_gate = Gate::from_env();
        assert!(!env_gate.is_local());
        let gate = gate("");
        let denied = gate.admit(
            "GET",
            "/api/nav",
            &headers(&[("x-phext-local", "1"), ("phext-local", "1"), ("authorization", "Bearer local")]),
        );
        assert_eq!(denied.unwrap_err().status, 401);
        assert!(!gate.describe().iter().any(|line| line.contains("DISABLED")));
    }

    #[test]
    fn query_values_are_percent_decoded() {
        let q = query_map("next=%2F%23a&code=ok&bad=%zz");
        assert_eq!(q["next"], "/#a");
        assert_eq!(q["code"], "ok");
        assert_eq!(q["bad"], "%zz");
    }

    fn written(reply: &Reply) -> String {
        let mut buf = Vec::new();
        reply.write_to(&mut buf);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn auth_responses_stay_private_and_carry_security_headers() {
        let gate = gate("github:100");
        let text = written(&gate.route("GET", "/api/session", &headers(&[])).unwrap());
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Cache-Control: no-store\r\n"));
        assert!(text.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(text.contains("X-Frame-Options: DENY\r\n"));
        assert!(text.contains("Referrer-Policy: strict-origin-when-cross-origin\r\n"));
        assert!(text.contains("Permissions-Policy: accelerometer=()"));
        assert!(text.contains("Content-Security-Policy: "));
        assert!(text.contains("frame-ancestors 'none'"));
        assert!(text.contains("script-src 'self' 'unsafe-inline'"));
        assert!(!text.contains("unsafe-eval"));
        assert!(!text.to_ascii_lowercase().contains("access-control-allow-origin"));
        assert!(!text.contains("Strict-Transport-Security"));
        assert!(!text.contains("upgrade-insecure-requests"));
    }

    #[test]
    fn https_forwarding_adds_hsts_to_auth_responses() {
        let gate = gate("");
        let text = written(
            &gate
                .route("GET", "/api/session", &headers(&[("x-forwarded-proto", "https")]))
                .unwrap(),
        );
        assert!(text.contains(&format!("Strict-Transport-Security: {HSTS_VALUE}\r\n")));
        assert!(text.contains("upgrade-insecure-requests"));
    }


    #[test]
    fn oauth_start_is_rate_limited_per_address() {
        let gate = gate("github:100");
        let h = headers(&[("x-real-ip", "203.0.113.4")]);
        for _ in 0..AUTH_PER_IP {
            assert_eq!(gate.route("GET", "/api/auth/github", &h).unwrap().status, 302);
        }
        let blocked = gate.route("GET", "/api/auth/github", &h).unwrap();
        assert_eq!(blocked.status, 429);
        assert!(written(&blocked).contains("Retry-After: 600\r\n"));
        assert!(written(&blocked).contains("Cache-Control: no-store\r\n"));
    }

    #[test]
    fn client_key_trusts_real_ip_then_the_appended_forwarded_hop() {
        let both = headers(&[
            ("x-real-ip", "203.0.113.8"),
            ("x-forwarded-for", "1.2.3.4, 203.0.113.9"),
        ]);
        assert_eq!(client_key(&both), "203.0.113.8");
        let forwarded = headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9")]);
        assert_eq!(client_key(&forwarded), "203.0.113.9");
        assert_eq!(client_key(&headers(&[])), "local");
    }

    const DESKTOP_STATE: &str = "attempt-state-0123456789abcdef";
    const DESKTOP_RETURN: &str = "http://127.0.0.1:8912/";

    /// Drive a desktop mint for a signed-in scripted session and
    /// hand back the assertion the loopback redirect carries.
    fn desktop_assertion(gate: &Gate, session: &str) -> String {
        let path = format!(
            "/api/auth/sso/desktop?return={}&state={DESKTOP_STATE}",
            form_encode(DESKTOP_RETURN)
        );
        let jump = gate.route("GET", &path, &headers(&[("cookie", session)])).unwrap();
        assert_eq!(jump.status, 302);
        let location = header(&jump, "Location")[0].to_string();
        let rest = location
            .strip_prefix(&format!("{DESKTOP_RETURN}?assertion="))
            .unwrap_or_else(|| panic!("unexpected redirect: {location}"));
        let (assertion, state) = rest.split_once("&state=").unwrap();
        assert_eq!(state, DESKTOP_STATE);
        assertion.to_string()
    }

    fn exchange(gate: &Gate, assertion: &str, state: &str) -> Reply {
        gate.route(
            "POST",
            "/api/auth/desktop/exchange",
            &headers(&[
                ("x-phext-assertion", assertion),
                ("x-phext-state", state),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn loopback_return_admits_only_ip_literals_with_ports() {
        for good in [
            "http://127.0.0.1:8912/",
            "http://127.0.0.1:1",
            "http://[::1]:65535/callback",
            "http://127.0.0.1:40000/?from=notepad",
        ] {
            assert_eq!(loopback_return(good).as_deref(), Some(good), "{good}");
        }
        for bad in [
            "",
            "http://localhost:8912/",       // a name, not the literal
            "http://127.0.0.2:8912/",        // another loopback address
            "http://127.1:8912/",            // shorthand loopback
            "http://0.0.0.0:8912/",          // the wildcard, not loopback
            "https://127.0.0.1:8912/",       // no CA certifies loopback
            "http://127.0.0.1/",             // no port: never a listener
            "http://127.0.0.1:0/",           // port zero is not a port
            "http://127.0.0.1:65536/",       // out of range
            "http://127.0.0.1:8912.evil.example/",
            "http://user@127.0.0.1:8912/",   // userinfo
            "http://127.0.0.1:8912/#frag",   // a fragment is not a listener
            "http://example.com:8912/",      // off this machine entirely
            "http://127.0.0.1:8912/a b",     // whitespace
            "//127.0.0.1:8912/",             // no scheme
        ] {
            assert_eq!(loopback_return(bad), None, "{bad}");
        }
    }

    #[test]
    fn sso_desktop_mints_an_assertion_that_carries_the_state() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");
        let assertion = desktop_assertion(&gate, &session);
        let body = open_assertion("test-sso-secret-must-be-32-chars-minimum", &assertion);
        assert_eq!(body["aud"], "desktop");
        assert_eq!(body["who"], "google:sub-1");
        assert_eq!(body["handle"], "ada");
        assert_eq!(body["editor"], true);
        assert_eq!(body["ret"], DESKTOP_RETURN);
        assert_eq!(body["st"], DESKTOP_STATE);
        assert_eq!(body["exp"].as_i64().unwrap() - body["iat"].as_i64().unwrap(), 120);
        assert!(!body["jti"].as_str().unwrap().is_empty());
        // The session secret does not open it: the keys never substitute.
        let (payload, sig) = assertion.split_once('.').unwrap();
        assert!(!verify("test-session-secret-must-be-32chars", payload, sig));
    }

    #[test]
    fn sso_desktop_sends_the_signed_out_to_sign_in_with_itself_as_next() {
        let gate = gate("");
        let path = format!(
            "/api/auth/sso/desktop?return={}&state={DESKTOP_STATE}",
            form_encode(DESKTOP_RETURN)
        );
        let reply = gate.route("GET", &path, &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
        let location = header(&reply, "Location")[0];
        let next = location.strip_prefix("http://127.0.0.1:8080/?next=").unwrap();
        assert_eq!(
            percent_decode(&percent_decode(next)),
            format!("/api/auth/sso/desktop?return={DESKTOP_RETURN}&state={DESKTOP_STATE}")
        );
    }

    #[test]
    fn sso_desktop_refuses_bad_returns_and_states_before_session_work() {
        // Signed out throughout: a bad return or state is a 400, not
        // the sign-in bounce — the bounce never carries them.
        let gate = gate("");
        for path in [
            "/api/auth/sso/desktop?return=http://example.com:8912/&state=attempt-state-0123456789abcdef",
            "/api/auth/sso/desktop?return=http://localhost:8912/&state=attempt-state-0123456789abcdef",
            "/api/auth/sso/desktop?return=https://127.0.0.1:8912/&state=attempt-state-0123456789abcdef",
            "/api/auth/sso/desktop?return=http://127.0.0.1:8912/",
            "/api/auth/sso/desktop?return=http://127.0.0.1:8912/&state=short",
            "/api/auth/sso/desktop?state=attempt-state-0123456789abcdef",
        ] {
            let reply = gate.route("GET", path, &headers(&[])).unwrap();
            assert_eq!(reply.status, 400, "{path}");
        }
    }

    #[test]
    fn sso_desktop_is_404_until_armed() {
        // Secret set, web audiences configured, desktop flag off:
        // both desktop endpoints answer as if they do not exist,
        // while the web audiences work exactly as before.
        let oauth = Oauth::from_parts(
            "http://127.0.0.1:8080",
            "test-session-secret-must-be-32chars",
            true,
            "github-client-id",
            "github-client-secret",
            "google-client-id",
            "google-client-secret",
            "x-client-id",
            "x-client-secret",
            "test-sso-secret-must-be-32-chars-minimum",
            "",
            "https://exocortexia.example",
            "https://wishnode.example",
            false,
            Arc::new(ScriptedSource),
        );
        assert!(!oauth.sso_desktop_ready());
        let gate = Gate::new(oauth, Editors::parse(""));
        let session = scripted_session(&gate, "google", "/");
        let path = format!(
            "/api/auth/sso/desktop?return={}&state={DESKTOP_STATE}",
            form_encode(DESKTOP_RETURN)
        );
        let reply = gate.route("GET", &path, &headers(&[("cookie", &session)])).unwrap();
        assert_eq!(reply.status, 404);
        let reply = exchange(&gate, "whatever", DESKTOP_STATE);
        assert_eq!(reply.status, 404);
        let reply = gate.route("GET", "/api/auth/sso/exocortexia", &headers(&[])).unwrap();
        assert_eq!(reply.status, 302);
    }

    #[test]
    fn desktop_exchange_issues_a_session_the_gate_admits() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");
        let assertion = desktop_assertion(&gate, &session);
        let reply = exchange(&gate, &assertion, DESKTOP_STATE);
        assert_eq!(reply.status, 200);
        let granted: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&reply.body).unwrap()).unwrap();
        let value = granted["session"].as_str().unwrap();
        let opened = gate
            .oauth
            .read_session(&format!("phext_session={value}"))
            .expect("a real session");
        assert_eq!(opened.who, "google:sub-1");
        assert_eq!(opened.handle, "ada");
        assert_eq!(granted["csrf"], opened.csrf);
        // And the gate admits it like any cookie the browser flow set.
        let cookie = format!("phext_session={value}");
        assert!(gate.admit("GET", "/api/status", &headers(&[("cookie", &cookie)])).is_ok());
    }

    #[test]
    fn desktop_exchange_burns_the_assertion() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");
        let assertion = desktop_assertion(&gate, &session);
        assert_eq!(exchange(&gate, &assertion, DESKTOP_STATE).status, 200);
        let replay = exchange(&gate, &assertion, DESKTOP_STATE);
        assert_eq!(replay.status, 401);
        assert!(written(&replay).contains("Sign-in did not complete."));
    }

    #[test]
    fn desktop_exchange_demands_the_attempts_own_state() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");
        let assertion = desktop_assertion(&gate, &session);
        // A wrong state is not this attempt's exchange: refused, and
        // the assertion is NOT spent — the real attempt can still land.
        let wrong = exchange(&gate, &assertion, "attempt-state-9999999999999999");
        assert_eq!(wrong.status, 401);
        assert_eq!(exchange(&gate, &assertion, DESKTOP_STATE).status, 200);
    }

    #[test]
    fn desktop_exchange_rejects_tampered_expired_and_foreign_assertions() {
        let gate = gate("");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");

        // Tampered payload: the seal no longer verifies.
        let assertion = desktop_assertion(&gate, &session);
        let (payload, sig) = assertion.split_once('.').unwrap();
        let mut bytes = payload.as_bytes().to_vec();
        bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
        let tampered = format!("{}.{}", String::from_utf8(bytes).unwrap(), sig);
        assert_eq!(exchange(&gate, &tampered, DESKTOP_STATE).status, 401);

        // Expired: honestly sealed under the SSO secret, but its
        // 120 seconds are long past.
        let expired = gate
            .oauth
            .seal_with(
                "test-sso-secret-must-be-32-chars-minimum",
                &SsoAssertionBody {
                    aud: "desktop".into(),
                    who: "google:sub-1".into(),
                    handle: "ada".into(),
                    name: "Ada Example".into(),
                    email: "ada@example.com".into(),
                    editor: true,
                    ret: DESKTOP_RETURN.into(),
                    st: Some(DESKTOP_STATE.into()),
                    iat: now() - 1000,
                    exp: now() - 880,
                    jti: "an-already-dead-jti".into(),
                },
            )
            .unwrap();
        assert_eq!(exchange(&gate, &expired, DESKTOP_STATE).status, 401);

        // Foreign: a Mutable assertion is genuine, sealed, unexpired —
        // and minted for a different audience. Not exchangeable.
        let jump = gate
            .route("GET", "/api/auth/sso/mutable", &headers(&[("cookie", &session)]))
            .unwrap();
        let location = header(&jump, "Location")[0].to_string();
        let foreign = location.split("assertion=").nth(1).unwrap().to_string();
        assert_eq!(exchange(&gate, &foreign, DESKTOP_STATE).status, 401);

        // Garbage is garbage.
        assert_eq!(exchange(&gate, "not-an-assertion", DESKTOP_STATE).status, 401);
        assert_eq!(exchange(&gate, "", DESKTOP_STATE).status, 401);
    }

    #[test]
    fn desktop_exchange_refuses_a_non_editor_and_still_burns() {
        // Only GitHub's octocat may edit; the scripted Google user
        // may sign in — and mint — but must not come away with an
        // editing session. The refusal is the gate's own sentence,
        // carries no credential, and spends the assertion.
        let gate = gate("github:100");
        let session = scripted_session(&gate, "google", "/api/auth/sso/desktop");
        let assertion = desktop_assertion(&gate, &session);
        let reply = exchange(&gate, &assertion, DESKTOP_STATE);
        assert_eq!(reply.status, 403);
        let text = String::from_utf8_lossy(&reply.body);
        assert!(text.contains("This account is not on the editor list."));
        assert!(!text.contains("\"session\""));
        assert_eq!(exchange(&gate, &assertion, DESKTOP_STATE).status, 401);
    }
}
