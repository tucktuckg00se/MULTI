//! `multi serve`: the web GUI and its JSON API, on top of [`Service`].
//!
//! Routes: `/` (the page), `/api/config` (GET/PUT), `/api/config/default`,
//! `/api/start`, `/api/stop`, `/api/status`, `/api/events` (SSE), `/api/me`,
//! `/login`, `/logout`, `/setup`.
//!
//! Security:
//! - People sign in with `web.username` and the password whose argon2id hash
//!   is `web.password_hash` (set by `multi passwd` or `/setup`), which gives a
//!   server-side session cookie. Once a password is set, every client must
//!   sign in, loopback included.
//! - Scripts send `Authorization: Bearer <token>` (`MULTI_WEB_TOKEN`, else
//!   `web.token`).
//! - With no password, a loopback client on a loopback bind has full access
//!   (the page suggests setting one). `/setup` creates the first password and
//!   only answers loopback clients with a loopback `Host`.
//! - On a loopback bind the `Host` header must name a loopback host (DNS
//!   rebinding). Every POST/PUT under `/api` must carry `X-Multi: 1`, which a
//!   cross-site form cannot send; `/login`, `/logout` and `/setup` refuse a
//!   foreign `Origin`.
//! - Secrets (stream keys, passphrases, the token, the password hash) are
//!   masked in every response; a masked value sent back keeps the stored one.

use std::convert::Infallible;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Form, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use multi_core::Config;
use multi_core::config::Issue;
use multi_media::url::redact;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

#[path = "web_outputs.rs"]
mod outputs;

use crate::auth::{self, Clock, Limiter, Sessions, eq_ct};
use crate::service::{
    ApplyReport, Effect, Service, ServiceStatus, Workers, changed_paths, effect_of,
};

/// Shown instead of the token and the password hash.
pub const MASK: &str = "********";
pub const TOKEN_ENV: &str = "MULTI_WEB_TOKEN";
const COOKIE: &str = "multi_session";

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const LOGIN_HTML: &str = include_str!("../web/login.html");
const SETUP_HTML: &str = include_str!("../web/setup.html");

/// Shared state of the web server.
#[derive(Clone)]
pub struct AppState {
    pub service: Service,
    store: Arc<Mutex<Config>>,
    path: Arc<PathBuf>,
    /// Address the server was started on (changes need a server restart).
    web: (IpAddr, u16),
    /// Serving HTTPS (cookies get `Secure`).
    tls: bool,
    /// Token from the environment; wins over `web.token`.
    env_token: Option<String>,
    sessions: Arc<Sessions>,
    limiter: Arc<Limiter>,
    shutdown: watch::Receiver<bool>,
}

impl AppState {
    /// `config` is what `path` holds; `web` is the bound address.
    pub fn new(
        service: Service,
        config: Config,
        path: PathBuf,
        env_token: Option<String>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self::with_clock(
            service,
            config,
            path,
            env_token,
            shutdown,
            auth::system_clock(),
        )
    }

    /// Like [`AppState::new`] with the clock that session expiry and sign-in
    /// back-off use.
    pub fn with_clock(
        service: Service,
        config: Config,
        path: PathBuf,
        env_token: Option<String>,
        shutdown: watch::Receiver<bool>,
        clock: Clock,
    ) -> Self {
        let web = (config.web.bind, config.web.port);
        let tls = config.web.tls_enabled();
        Self {
            service,
            store: Arc::new(Mutex::new(config)),
            path: Arc::new(path),
            web,
            tls,
            env_token: env_token.filter(|t| !t.is_empty()),
            sessions: Arc::new(Sessions::new(clock.clone())),
            limiter: Arc::new(Limiter::new(clock)),
            shutdown,
        }
    }

    fn config(&self) -> Config {
        self.store
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn local_bind(&self) -> bool {
        self.web.0.is_loopback()
    }

    fn token(&self) -> Option<String> {
        self.env_token
            .clone()
            .or_else(|| self.config().web.token.filter(|t| !t.is_empty()))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/setup", get(setup_page).post(setup))
        .route("/api/me", get(me))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/default", get(default_config))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/outputs/{index}/start", post(outputs::start))
        .route("/api/outputs/{index}/stop", post(outputs::stop))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

// ---------------------------------------------------------------- security

/// How a request is allowed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Access {
    /// Signed in with the password.
    Session,
    /// `Authorization: Bearer <token>`.
    Token,
    /// No password set, loopback client on a loopback bind.
    Local,
}

fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    }
}

fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = host_name(host);
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A browser form POST from another site carries a foreign `Origin` (or
/// `Sec-Fetch-Site: cross-site`); scripts send neither.
fn same_origin(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site" || v == "same-site")
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin.split_once("://").map(|(_, rest)| rest);
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

/// The client's address, set by [`guard`]; `None` when unknown (then never
/// treated as loopback).
#[derive(Clone, Copy)]
struct Peer(Option<IpAddr>);

fn peer(req_ext: &axum::http::Extensions) -> Option<IpAddr> {
    req_ext
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| a.ip().to_canonical())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn access(state: &AppState, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<Access> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if let (Some(b), Some(token)) = (bearer, state.token())
        && eq_ct(b.trim().as_bytes(), token.as_bytes())
    {
        return Some(Access::Token);
    }
    if cookie_value(headers, COOKIE).is_some_and(|id| state.sessions.check(id)) {
        return Some(Access::Session);
    }
    let local_peer = peer.is_some_and(|p| p.is_loopback());
    if !state.config().web.has_password() && state.local_bind() && local_peer {
        return Some(Access::Local);
    }
    None
}

async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let api = path.starts_with("/api/");
    let write = matches!(*req.method(), Method::POST | Method::PUT);
    if api && write && req.headers().get("x-multi").is_none_or(|v| v != "1") {
        return ApiError(StatusCode::FORBIDDEN, "missing X-Multi: 1 header".into()).into_response();
    }
    if state.local_bind() && !loopback_host(req.headers()) {
        return ApiError(StatusCode::FORBIDDEN, "host not allowed".into()).into_response();
    }
    if !api && write && !same_origin(req.headers()) {
        return ApiError(StatusCode::FORBIDDEN, "cross-site request refused".into())
            .into_response();
    }
    let client = peer(req.extensions());
    req.extensions_mut().insert(Peer(client));
    if let Some(a) = access(&state, req.headers(), client) {
        req.extensions_mut().insert(a);
        return next.run(req).await;
    }
    if matches!(path.as_str(), "/login" | "/logout" | "/setup" | "/app.css") {
        return next.run(req).await;
    }
    if api {
        ApiError(StatusCode::UNAUTHORIZED, "sign in required".into()).into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn err_html(msg: &str) -> String {
    format!("<p class=\"err\" role=\"alert\">{}</p>", esc(msg))
}

fn login_html(state: &AppState, error: &str, peer: Option<IpAddr>) -> String {
    let config = state.config();
    let (lede, form) = if config.web.has_password() {
        (
            "Sign in to change settings and start or stop captioning.".to_string(),
            format!(
                "<form method=\"post\" action=\"/login\">\
<label class=\"label\" for=\"username\">User name</label>\
<input id=\"username\" name=\"username\" type=\"text\" autocomplete=\"username\" value=\"{}\" required>\
<label class=\"label\" for=\"password\">Password</label>\
<input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required autofocus>\
<button class=\"primary\" type=\"submit\">Sign in</button></form>",
                esc(&config.web.username)
            ),
        )
    } else {
        let here = if peer.is_some_and(|p| p.is_loopback()) {
            " or <a href=\"/setup\">create one now</a>"
        } else {
            " or from this machine at <code>/setup</code>"
        };
        (
            format!(
                "No password is set yet. Set one with <code>multi passwd -c &lt;config&gt;</code>{here}."
            ),
            String::new(),
        )
    };
    LOGIN_HTML
        .replace("{{lede}}", &lede)
        .replace("{{error}}", error)
        .replace("{{form}}", &form)
}

async fn login_page(
    State(state): State<AppState>,
    Extension(Peer(p)): Extension<Peer>,
) -> Html<String> {
    Html(login_html(&state, "", p))
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    password: String,
}

fn session_cookie(state: &AppState, id: &str, max_age: u64) -> Option<HeaderValue> {
    let secure = if state.tls { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{secure}"
    ))
    .ok()
}

/// Redirects to `/` with a new session cookie.
fn signed_in(state: &AppState) -> Response {
    let id = match state.sessions.create() {
        Ok(id) => id,
        Err(e) => {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
    };
    let mut resp = Redirect::to("/").into_response();
    if let Some(v) = session_cookie(state, &id, auth::ABSOLUTE_TIMEOUT.as_secs()) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn login(
    State(state): State<AppState>,
    Extension(Peer(peer)): Extension<Peer>,
    Form(f): Form<LoginForm>,
) -> Response {
    // Unknown peers share one bucket.
    let ip = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let page = |code: StatusCode, msg: &str| {
        (code, Html(login_html(&state, &err_html(msg), peer))).into_response()
    };
    if let Some(wait) = state.limiter.blocked(ip) {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            &format!(
                "Too many failed sign-ins. Try again in {}.",
                auth::human(wait)
            ),
        );
    }
    let config = state.config();
    let Some(hash) = config.web.password_hash.clone().filter(|h| !h.is_empty()) else {
        return page(StatusCode::UNAUTHORIZED, "No password is set yet.");
    };
    let password = f.password;
    let pw_ok = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    let user_ok = eq_ct(f.username.trim().as_bytes(), config.web.username.as_bytes());
    if !(pw_ok && user_ok) {
        warn!(client = %ip, "web sign-in failed");
        let msg = match state.limiter.fail(ip) {
            Some(wait) => format!(
                "Wrong user name or password. Too many failed sign-ins: try again in {}.",
                auth::human(wait)
            ),
            None => "Wrong user name or password.".to_string(),
        };
        return page(StatusCode::UNAUTHORIZED, &msg);
    }
    state.limiter.clear(ip);
    info!(client = %ip, "web sign-in");
    signed_in(&state)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie_value(&headers, COOKIE) {
        state.sessions.remove(id);
    }
    let mut resp = Redirect::to("/login").into_response();
    if let Some(v) = session_cookie(&state, "", 0) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

#[derive(Serialize)]
struct Me {
    via: Access,
    user: Option<String>,
    password_set: bool,
    tls: bool,
}

async fn me(State(state): State<AppState>, access: Option<Extension<Access>>) -> Json<Me> {
    let config = state.config();
    let via = access.map_or(Access::Local, |Extension(a)| a);
    Json(Me {
        via,
        user: (via == Access::Session).then(|| config.web.username.clone()),
        password_set: config.web.has_password(),
        tls: state.tls,
    })
}

/// Why `/setup` is unavailable to this request, if it is.
fn setup_refusal(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Option<&'static str> {
    if state.config().web.has_password() {
        return Some("A password is already set. Sign in, or change it with multi passwd.");
    }
    if !peer.is_some_and(|p| p.is_loopback()) || !loopback_host(headers) {
        return Some(
            "Set a password with multi passwd, or open this page on the machine running MULTI (http(s)://localhost).",
        );
    }
    None
}

fn setup_html(state: &AppState, error: &str) -> String {
    SETUP_HTML
        .replace("{{error}}", error)
        .replace("{{username}}", &esc(&state.config().web.username))
        .replace("{{min}}", &auth::MIN_PASSWORD_CHARS.to_string())
}

async fn setup_page(
    State(state): State<AppState>,
    Extension(Peer(peer)): Extension<Peer>,
    headers: HeaderMap,
) -> Response {
    match setup_refusal(&state, &headers, peer) {
        Some(why) => (
            StatusCode::FORBIDDEN,
            Html(
                LOGIN_HTML
                    .replace("{{lede}}", "First-run setup")
                    .replace("{{error}}", &err_html(why))
                    .replace("{{form}}", "<p><a href=\"/login\">Sign in</a></p>"),
            ),
        )
            .into_response(),
        None => Html(setup_html(&state, "")).into_response(),
    }
}

#[derive(Deserialize)]
struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

async fn setup(
    State(state): State<AppState>,
    Extension(Peer(peer)): Extension<Peer>,
    headers: HeaderMap,
    Form(f): Form<SetupForm>,
) -> Response {
    if let Some(why) = setup_refusal(&state, &headers, peer) {
        return ApiError(StatusCode::FORBIDDEN, why.into()).into_response();
    }
    let bad = |msg: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(setup_html(&state, &err_html(msg))),
        )
            .into_response()
    };
    let username = f.username.trim().to_string();
    if username.is_empty() {
        return bad("Enter a user name.");
    }
    if f.password != f.confirm {
        return bad("The passwords do not match.");
    }
    if let Err(e) = auth::check_new_password(&f.password) {
        return bad(&format!("{e}"));
    }
    let st = state.clone();
    let saved = tokio::task::spawn_blocking(move || -> Result<bool> {
        let hash = auth::hash_password(&f.password)?;
        let mut store = st.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.web.has_password() {
            return Ok(false); // lost a race with another setup
        }
        let mut new = store.clone();
        new.web.username = username;
        new.web.password_hash = Some(hash);
        write_atomic(&st.path, &new.to_toml()?)
            .with_context(|| format!("cannot save {}", st.path.display()))?;
        *store = new;
        Ok(true)
    })
    .await;
    match saved {
        Ok(Ok(true)) => {
            info!("web password created on the first-run page");
            signed_in(&state)
        }
        Ok(Ok(false)) => Redirect::to("/login").into_response(),
        Ok(Err(e)) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------- errors

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

#[derive(Serialize)]
struct Issues {
    issues: Vec<Issue>,
}

fn unprocessable(issues: Vec<Issue>) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(Issues { issues })).into_response()
}

// ---------------------------------------------------------------- secrets

/// The config with stream keys, passphrases, the token and the password hash hidden.
pub fn mask(c: &Config) -> Config {
    let mut m = c.clone();
    m.input.url = redact(&m.input.url);
    for o in &mut m.outputs {
        o.url = redact(&o.url);
    }
    if m.web.token.as_deref().is_some_and(|t| !t.is_empty()) {
        m.web.token = Some(MASK.into());
    }
    if m.web.has_password() {
        m.web.password_hash = Some(MASK.into());
    }
    m
}

fn restore<'a>(url: &mut String, mut candidates: impl Iterator<Item = &'a String>) {
    if url.contains("***")
        && let Some(s) = candidates.find(|s| redact(s) == *url)
    {
        *url = s.clone();
    }
}

/// Puts stored secrets back where `new` still has the masked form.
pub fn unmask(new: &mut Config, stored: &Config) {
    restore(&mut new.input.url, std::iter::once(&stored.input.url));
    for o in &mut new.outputs {
        restore(&mut o.url, stored.outputs.iter().map(|s| &s.url));
    }
    if new.web.token.as_deref() == Some(MASK) {
        new.web.token = stored.web.token.clone();
    }
    // The password changes only through `multi passwd` or `/setup`.
    new.web.password_hash = stored.web.password_hash.clone();
}

/// Checks beyond [`Config::validate`] that only the server knows about.
fn server_issues(state: &AppState, c: &Config) -> Vec<Issue> {
    let mut v = Vec::new();
    let mut hidden = |path: String, url: &str| {
        if url.contains("***") {
            v.push(Issue {
                path,
                message: "re-enter the hidden stream key or passphrase".into(),
            });
        }
    };
    hidden("input.url".into(), &c.input.url);
    for (i, o) in c.outputs.iter().enumerate() {
        hidden(format!("outputs[{i}].url"), &o.url);
    }
    let has_token =
        state.env_token.is_some() || c.web.token.as_deref().is_some_and(|t| !t.is_empty());
    if (!c.web.bind.is_loopback() || !state.local_bind()) && !has_token && !c.web.has_password() {
        v.push(Issue {
            path: "web.token".into(),
            message: format!(
                "the GUI is reachable from other machines: set a password (multi passwd) or a token (or {TOKEN_ENV})"
            ),
        });
    }
    v
}

/// Writes `text` next to `path` and renames it into place (mode 0600: the
/// file may hold stream keys).
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------- handlers

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

async fn get_config(State(state): State<AppState>) -> Json<Config> {
    Json(mask(&state.config()))
}

async fn default_config() -> Json<Config> {
    Json(Config::default())
}

#[derive(Serialize)]
struct Saved {
    report: ApplyReport,
    config: Config,
}

async fn put_config(State(state): State<AppState>, body: Bytes) -> Response {
    let de = &mut serde_json::Deserializer::from_slice(&body);
    let mut new: Config = match serde_path_to_error::deserialize(de) {
        Ok(c) => c,
        Err(e) => {
            let path = e.path().to_string();
            return unprocessable(vec![Issue {
                path: if path == "." { String::new() } else { path },
                message: e.into_inner().to_string(),
            }]);
        }
    };
    let mut stored = state.config();
    // `multi passwd` may have changed the file since: never overwrite its hash.
    let path = state.path.clone();
    if let Ok(Ok(disk)) = tokio::task::spawn_blocking(move || Config::load(&path)).await {
        stored.web.password_hash = disk.web.password_hash;
    }
    unmask(&mut new, &stored);
    let mut issues = new.validate();
    issues.extend(server_issues(&state, &new));
    if !issues.is_empty() {
        return unprocessable(issues);
    }
    let text = match new.to_toml() {
        Ok(t) => t,
        Err(e) => {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let path = state.path.clone();
    let written = tokio::task::spawn_blocking(move || write_atomic(&path, &text)).await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let msg = format!("cannot save {}: {e}", state.path.display());
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
        }
        Err(e) => {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    if let Ok(mut c) = state.store.lock() {
        *c = new.clone();
    }
    let report = state.service.apply(new.clone());
    info!(
        live = ?report.live,
        restart = ?report.restart,
        server_restart = ?report.server_restart,
        "configuration saved"
    );
    Json(Saved {
        report,
        config: mask(&new),
    })
    .into_response()
}

#[derive(Serialize)]
struct StatusOut {
    #[serde(flatten)]
    status: ServiceStatus,
    config_issues: Vec<Issue>,
    /// Advice that doesn't block starting: config warnings and live conditions.
    warnings: Vec<Issue>,
}

/// Seconds of silent input audio (while video flows) before the GUI warns.
const SILENCE_WARN_S: f64 = 10.0;

/// Warnings about the running pipeline, as opposed to the saved config.
fn live_warnings(status: &ServiceStatus) -> Vec<Issue> {
    let mut out = Vec::new();
    if status.state != crate::service::RunState::Running {
        return out;
    }
    if let Some(m) = &status.media
        && m.input_live
        && m.audio_silent_s.is_some_and(|s| s >= SILENCE_WARN_S)
    {
        out.push(Issue {
            path: "audio".into(),
            message: format!(
                "Input audio has been silent (below {} dBFS) for over {} s, so there is no speech to caption. Check the source: is the microphone connected, unmuted and on the streamed audio track?",
                multi_media::SILENCE_DBFS,
                SILENCE_WARN_S
            ),
        });
    }
    out
}

fn full_status(state: &AppState) -> StatusOut {
    let mut status = state.service.status();
    let config = state.config();
    let mut running_web = config.clone();
    running_web.web.bind = state.web.0;
    running_web.web.port = state.web.1;
    status.server_restart_pending = changed_paths(&running_web, &config)
        .into_iter()
        .filter(|p| effect_of(p) == Effect::Server)
        .collect();
    let mut config_issues = config.validate();
    config_issues.extend(server_issues(state, &config));
    let mut warnings = config.warnings();
    warnings.extend(live_warnings(&status));
    warnings.extend(state.service.missing_models(&config));
    StatusOut {
        status,
        config_issues,
        warnings,
    }
}

async fn status(State(state): State<AppState>) -> Json<StatusOut> {
    Json(full_status(&state))
}

async fn start(State(state): State<AppState>) -> Response {
    let config = state.config();
    let mut issues = config.validate();
    issues.extend(server_issues(&state, &config));
    if !issues.is_empty() {
        return unprocessable(issues);
    }
    let svc = state.service.clone();
    match tokio::task::spawn_blocking(move || svc.start(config)).await {
        Ok(Ok(())) => Json(full_status(&state)).into_response(),
        Ok(Err(e)) => {
            let code = if svc_running(&state) {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            ApiError(code, format!("{e:#}")).into_response()
        }
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn svc_running(state: &AppState) -> bool {
    state.service.is_active()
}

async fn stop(State(state): State<AppState>) -> Response {
    let svc = state.service.clone();
    // A pipeline that ended with an error is reported in `errors`.
    if let Err(e) = tokio::task::spawn_blocking(move || svc.stop()).await {
        return ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(full_status(&state)).into_response()
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.service.events();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    return Some((Ok(SseEvent::default().data(data)), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let mut shutdown = state.shutdown.clone();
    let stream = stream.take_until(async move {
        let _ = shutdown.wait_for(|s| *s).await;
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------- serve

/// Loads `path`, or writes the defaults there if it does not exist.
pub fn load_or_create(path: &Path) -> Result<Config> {
    if path.exists() {
        return Config::load(path).context("loading configuration");
    }
    let config = Config::default();
    write_atomic(path, &config.to_toml()?)
        .with_context(|| format!("cannot create {}", path.display()))?;
    info!(path = %path.display(), "created configuration with defaults");
    Ok(config)
}

/// `multi serve`: the service plus the web server until Ctrl-C/SIGTERM.
pub fn serve(path: &Path, workers: Workers) -> Result<()> {
    let config = load_or_create(path)?;
    let env_token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    let has_token =
        env_token.is_some() || config.web.token.as_deref().is_some_and(|t| !t.is_empty());
    if !config.web.bind.is_loopback() && !has_token && !config.web.has_password() {
        bail!(
            "web.bind {} is reachable from other machines: set a password with `multi passwd -c {}`, or set {TOKEN_ENV} or web.token",
            config.web.bind,
            path.display()
        );
    }
    let tls = if config.web.tls_enabled() {
        let t = crate::tls::load(&config.web, path)?;
        if t.generated {
            info!(cert = %t.cert_path.display(), "made a self-signed HTTPS certificate");
        }
        info!(
            cert = %t.cert_path.display(),
            "HTTPS certificate SHA-256 fingerprint {} (compare it with the browser's before accepting)",
            t.fingerprint
        );
        Some(t.config)
    } else {
        if !config.web.bind.is_loopback() {
            warn!(
                "web.tls = \"off\" on a network address: passwords and tokens cross the network in plain text"
            );
        }
        None
    };
    let service = Service::new(config.clone(), workers);
    service
        .spawn_ticker()
        .context("cannot start the status thread")?;
    if config.web.autostart {
        if config.validate().is_empty() {
            if let Err(e) = service.start(config.clone()) {
                warn!("autostart failed: {e:#}");
            }
        } else {
            warn!("autostart skipped: the configuration has problems");
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let (tx, rx) = watch::channel(false);
    let addr = SocketAddr::new(config.web.bind, config.web.port);
    if !config.web.has_password() {
        warn!(
            "no web password is set: set one with `multi passwd -c {}` or on the first-run page",
            path.display()
        );
    }
    let state = AppState::new(service.clone(), config, path.to_path_buf(), env_token, rx);
    let served = rt.block_on(async move {
        let listener = std::net::TcpListener::bind(addr)
            .with_context(|| format!("cannot listen on {addr}"))?;
        let scheme = if tls.is_some() { "https" } else { "http" };
        let shown = if addr.ip().is_unspecified() {
            format!("{scheme}://<this-host>:{}/", addr.port())
        } else {
            format!("{scheme}://{addr}/")
        };
        info!("web GUI at {shown}");
        let handle = axum_server::Handle::new();
        let h = handle.clone();
        tokio::spawn(async move {
            shutdown_signal(tx).await;
            h.graceful_shutdown(Some(Duration::from_secs(5)));
        });
        run_server(listener, router(state), tls, handle).await
    });
    info!("shutting down the pipeline");
    let stopped = service.stop();
    served.and(stopped)
}

/// Serves `app` on `listener`: HTTPS only when `tls` is given, else plain
/// HTTP. Handlers see the client address (`ConnectInfo<SocketAddr>`).
pub async fn run_server(
    listener: std::net::TcpListener,
    app: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    handle: axum_server::Handle<SocketAddr>,
) -> Result<()> {
    listener
        .set_nonblocking(true)
        .context("cannot configure the listener")?;
    let svc = app.into_make_service_with_connect_info::<SocketAddr>();
    let served = match tls {
        Some(cfg) => {
            let cfg = axum_server::tls_rustls::RustlsConfig::from_config(cfg);
            axum_server::from_tcp_rustls(listener, cfg)?
                .handle(handle)
                .serve(svc)
                .await
        }
        None => {
            axum_server::from_tcp(listener)?
                .handle(handle)
                .serve(svc)
                .await
        }
    };
    served.context("web server failed")
}

async fn shutdown_signal(tx: watch::Sender<bool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
    info!("stop requested");
    let _ = tx.send(true);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use multi_core::config::Output;
    use tower::ServiceExt;

    struct T {
        app: Router,
        state: AppState,
        _dir: TempDir,
        host: &'static str,
        /// Client address of `call`.
        peer: IpAddr,
        /// Seconds on the injected clock.
        now: Arc<std::sync::atomic::AtomicU64>,
        /// Sent as `Cookie` unless the call sets one.
        extra_cookie: Option<String>,
        _tx: watch::Sender<bool>,
    }

    fn running_with(silent_s: Option<f64>, live: bool) -> ServiceStatus {
        let media = multi_media::Stats {
            input_live: live,
            audio_silent_s: silent_s,
            ..Default::default()
        };
        ServiceStatus {
            state: crate::service::RunState::Running,
            uptime_s: Some(30),
            media: Some(media),
            workers: Vec::new(),
            caption_lag_ms: None,
            gpu: None,
            errors: Vec::new(),
            restart_pending: Vec::new(),
            server_restart_pending: Vec::new(),
        }
    }

    #[test]
    fn silence_warning_only_when_live_and_silent_long_enough() {
        assert_eq!(live_warnings(&running_with(Some(12.0), true)).len(), 1);
        assert!(live_warnings(&running_with(Some(3.0), true)).is_empty());
        assert!(live_warnings(&running_with(Some(60.0), false)).is_empty());
        assert!(live_warnings(&running_with(None, true)).is_empty());
        let mut stopped = running_with(Some(60.0), true);
        stopped.state = crate::service::RunState::Stopped;
        assert!(live_warnings(&stopped).is_empty());
    }

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup(config: Config, env_token: Option<&str>) -> T {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("multi-web-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("multi.toml");
        std::fs::write(&path, config.to_toml().unwrap()).unwrap();
        let (tx, rx) = watch::channel(false);
        let host = if config.web.bind.is_loopback() {
            "127.0.0.1:8480"
        } else {
            "10.0.0.5:8480"
        };
        let peer = if config.web.bind.is_loopback() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            IpAddr::from([10, 0, 0, 7])
        };
        let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let n2 = now.clone();
        let clock: Clock =
            Arc::new(move || Duration::from_secs(n2.load(std::sync::atomic::Ordering::SeqCst)));
        let service = Service::new(config.clone(), Workers::default());
        let state = AppState::with_clock(
            service,
            config,
            path,
            env_token.map(String::from),
            rx,
            clock,
        );
        T {
            app: router(state.clone()),
            state,
            _dir: TempDir(dir),
            host,
            peer,
            now,
            extra_cookie: None,
            _tx: tx,
        }
    }

    impl T {
        async fn call(
            &self,
            method: &str,
            uri: &str,
            body: Option<String>,
            extra: &[(&str, &str)],
        ) -> (StatusCode, HeaderMap, String) {
            self.call_from(self.peer, method, uri, body, extra).await
        }

        async fn call_from(
            &self,
            peer: IpAddr,
            method: &str,
            uri: &str,
            body: Option<String>,
            extra: &[(&str, &str)],
        ) -> (StatusCode, HeaderMap, String) {
            let mut b = Request::builder()
                .extension(ConnectInfo(SocketAddr::new(peer, 50_000)))
                .method(method)
                .uri(uri)
                .header("host", self.host)
                .header("x-multi", "1")
                .header("content-type", "application/json");
            if let Some(h) = b.headers_mut() {
                if let Some(c) = &self.extra_cookie {
                    h.insert(header::COOKIE, HeaderValue::from_str(c).unwrap());
                }
                for (k, v) in extra {
                    h.insert(
                        header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        HeaderValue::from_str(v).unwrap(),
                    );
                }
            }
            let req = b.body(body.map_or_else(Body::empty, Body::from)).unwrap();
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                headers,
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        }
    }

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn config_round_trip_saves_toml() {
        let t = setup(Config::default(), None);
        let (code, _, body) = t.call("GET", "/api/config", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        let mut c: Config = serde_json::from_str(&body).unwrap();
        assert_eq!(c, Config::default());
        c.captions.rows = 2;
        c.filter.blocklist = vec!["darn".into()];
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&c).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let v = json(&body);
        assert_eq!(
            v["report"]["live"],
            serde_json::json!(["captions.rows", "filter.blocklist"])
        );
        let saved = Config::load(&t.state.path).unwrap();
        assert_eq!(saved, c);
        let (_, _, body) = t.call("GET", "/api/config", None, &[]).await;
        assert_eq!(serde_json::from_str::<Config>(&body).unwrap(), c);
        let (code, _, body) = t.call("GET", "/api/config/default", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Config>(&body).unwrap(),
            Config::default()
        );
    }

    #[tokio::test]
    async fn bad_input_gives_422_with_paths() {
        let t = setup(Config::default(), None);
        let mut c = Config::default();
        c.captions.rows = 9;
        c.languages[2].cea708_service = Some(1);
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&c).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        let paths: Vec<String> = json(&body)["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"captions.rows".into()), "{paths:?}");
        assert!(
            paths.contains(&"languages[2].cea708_service".into()),
            "{paths:?}"
        );
        // Type errors carry the path too.
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(r#"{"captions":{"rows":"x"}}"#.into()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json(&body)["issues"][0]["path"], "captions.rows");
        // Nothing was saved.
        assert_eq!(Config::load(&t.state.path).unwrap(), Config::default());
    }

    #[tokio::test]
    async fn secrets_are_masked_and_kept() {
        let mut c = Config::default();
        c.input.url = "srt://0.0.0.0:9000?mode=listener&passphrase=inputsecret".into();
        c.outputs = vec![
            Output::new("rtmp://a.rtmp.youtube.com/live2/abcd-efgh-key"),
            Output::new("srt://h:1?mode=caller&passphrase=outsecret"),
        ];
        c.web.token = Some("tok-123".into());
        c.web.password_hash = Some(auth::hash_password("hunter2hunter2").unwrap());
        let hash = c.web.password_hash.clone().unwrap();
        let t = setup(c.clone(), None);
        let (_, h, _) = t.login("admin", "hunter2hunter2").await;
        let cookie = cookie_of(&h);
        let t = T {
            extra_cookie: Some(cookie),
            ..t
        };
        let (_, _, body) = t.call("GET", "/api/config", None, &[]).await;
        let salt = hash.split('$').nth(4).unwrap();
        for secret in [
            "inputsecret",
            "abcd-efgh-key",
            "outsecret",
            "tok-123",
            "argon2",
            salt,
        ] {
            assert!(!body.contains(secret), "{secret} leaked: {body}");
        }
        // Send the masked config back with one change: secrets survive.
        let mut m: Config = serde_json::from_str(&body).unwrap();
        m.captions.rows = 2;
        m.outputs.reverse();
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&m).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(!body.contains("abcd-efgh-key"));
        let saved = Config::load(&t.state.path).unwrap();
        assert_eq!(saved.input.url, c.input.url);
        assert_eq!(saved.outputs[1].url, c.outputs[0].url);
        assert_eq!(saved.outputs[0].url, c.outputs[1].url);
        assert_eq!(saved.web.token.as_deref(), Some("tok-123"));
        assert_eq!(saved.web.password_hash.as_deref(), Some(hash.as_str()));
        // The API cannot change the password hash.
        let mut forged = m.clone();
        forged.web.password_hash = Some("$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHQ$aGFzaA".into());
        let (code, _, _) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&forged).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        let saved = Config::load(&t.state.path).unwrap();
        assert_eq!(saved.web.password_hash.as_deref(), Some(hash.as_str()));
        // A masked key on a changed URL cannot be restored: 422.
        m.outputs[1].url = "rtmp://other.example/live2/***".into();
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&m).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json(&body)["issues"][0]["path"], "outputs[1].url");
        // Status and errors never show secrets either.
        let (_, _, body) = t.call("GET", "/api/status", None, &[]).await;
        assert!(!body.contains("tok-123") && !body.contains("outsecret"));
    }

    #[tokio::test]
    async fn loopback_needs_no_token_but_a_local_host() {
        let t = setup(Config::default(), None);
        let (code, _, _) = t.call("GET", "/api/status", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("host", "evil.example:8480")])
            .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        // Mutations need the custom header.
        let req = Request::builder()
            .method("POST")
            .uri("/api/stop")
            .header("host", "localhost:8480")
            .body(Body::empty())
            .unwrap();
        let resp = t.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn token_is_enforced_off_loopback() {
        let mut c = Config::default();
        c.web.bind = "0.0.0.0".parse().unwrap();
        c.web.token = Some("from-file".into());
        let t = setup(c, Some("from-env"));
        let (code, _, _) = t.call("GET", "/api/config", None, &[]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, h, _) = t.call("GET", "/", None, &[]).await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert_eq!(h["location"], "/login");
        // The environment wins over the file.
        let (code, _, _) = t
            .call(
                "GET",
                "/api/config",
                None,
                &[("authorization", "Bearer from-file")],
            )
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _, _) = t
            .call(
                "GET",
                "/api/config",
                None,
                &[("authorization", "Bearer from-env")],
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        // People cannot sign in with the token, and without a password
        // the login page says how to set one.
        let (code, _, body) = t.call("GET", "/login", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("multi passwd") && !body.contains("name=\"password\""));
        let (code, _, _) = t.call("GET", "/api/me", None, &[]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

    fn with_password(mut c: Config, pw: &str) -> Config {
        c.web.password_hash = Some(auth::hash_password(pw).unwrap());
        c
    }

    fn cookie_of(h: &HeaderMap) -> String {
        h[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    impl T {
        async fn login(&self, user: &str, pw: &str) -> (StatusCode, HeaderMap, String) {
            let body = format!("username={user}&password={pw}");
            self.call("POST", "/login", Some(body), &[FORM]).await
        }
        fn advance(&self, secs: u64) {
            self.now
                .fetch_add(secs, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn password_login_session_logout_and_expiry() {
        let t = setup(with_password(Config::default(), "hunter2hunter2"), None);
        // A password means even loopback clients sign in.
        let (code, _, _) = t.call("GET", "/api/status", None, &[]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, h, _) = t.call("GET", "/", None, &[]).await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert_eq!(h["location"], "/login");
        let (code, _, body) = t.call("GET", "/login", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("name=\"password\"") && body.contains("value=\"admin\""));
        // Failures.
        let (code, h, body) = t.login("admin", "wrong-password").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert!(h.get(header::SET_COOKIE).is_none());
        assert!(body.contains("Wrong user name or password"));
        let (code, _, _) = t.login("root", "hunter2hunter2").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // Success: an HttpOnly, SameSite=Strict session cookie (no Secure
        // without TLS), random each time.
        let (code, h, _) = t.login("admin", "hunter2hunter2").await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert_eq!(h["location"], "/");
        let set = h[header::SET_COOKIE].to_str().unwrap().to_string();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
        assert!(!set.contains("Secure"), "{set}");
        let cookie = cookie_of(&h);
        assert_eq!(cookie.len(), "multi_session=".len() + 64);
        let (_, h2, _) = t.login("admin", "hunter2hunter2").await;
        assert_ne!(cookie_of(&h2), cookie);
        let (code, _, body) = t.call("GET", "/api/me", None, &[("cookie", &cookie)]).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(json(&body)["via"], "session");
        assert_eq!(json(&body)["user"], "admin");
        let (code, _, _) = t.call("GET", "/", None, &[("cookie", &cookie)]).await;
        assert_eq!(code, StatusCode::OK);
        // A forged cookie does nothing.
        let forged = format!("multi_session={}", "0".repeat(64));
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("cookie", &forged)])
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // Logout ends the session server-side.
        let (code, h, _) = t
            .call("POST", "/logout", None, &[("cookie", &cookie)])
            .await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert!(
            h[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("cookie", &cookie)])
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // Idle expiry (12 h) on the injected clock.
        let other = cookie_of(&h2);
        t.advance(11 * 3600);
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("cookie", &other)])
            .await;
        assert_eq!(code, StatusCode::OK);
        t.advance(12 * 3600 + 1);
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("cookie", &other)])
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // A cross-site form cannot sign in.
        let (code, _, _) = t
            .call(
                "POST",
                "/login",
                Some("username=admin&password=hunter2hunter2".into()),
                &[FORM, ("origin", "https://evil.example")],
            )
            .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn failed_logins_are_rate_limited_per_client() {
        let t = setup(with_password(Config::default(), "hunter2hunter2"), None);
        for i in 1..=5 {
            let (code, _, body) = t.login("admin", "nope-nope").await;
            assert_eq!(code, StatusCode::UNAUTHORIZED);
            assert_eq!(
                body.contains("try again in 30 s"),
                i == 5,
                "attempt {i}: {body}"
            );
        }
        // Locked out: even the right password is refused, without checking.
        let (code, _, body) = t.login("admin", "hunter2hunter2").await;
        assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("Too many failed sign-ins"), "{body}");
        // Another address is not affected.
        let (code, _, _) = t
            .call_from(
                IpAddr::from([127, 0, 0, 2]),
                "POST",
                "/login",
                Some("username=admin&password=hunter2hunter2".into()),
                &[FORM],
            )
            .await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        // The next failure doubles the wait; after it, the right one works.
        t.advance(30);
        let (_, _, body) = t.login("admin", "nope-nope").await;
        assert!(body.contains("try again in 1 min"), "{body}");
        t.advance(60);
        let (code, _, _) = t.login("admin", "hunter2hunter2").await;
        assert_eq!(code, StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn bearer_token_still_works_with_a_password() {
        let mut c = with_password(Config::default(), "hunter2hunter2");
        c.web.bind = "0.0.0.0".parse().unwrap();
        c.web.token = Some("script-token".into());
        let t = setup(c, None);
        let (code, _, _) = t.call("GET", "/api/status", None, &[]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _, body) = t
            .call(
                "GET",
                "/api/me",
                None,
                &[("authorization", "Bearer script-token")],
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(json(&body)["via"], "token");
        assert_eq!(json(&body)["tls"], true);
        let (code, _, _) = t
            .call(
                "POST",
                "/api/stop",
                None,
                &[("authorization", "Bearer script-token")],
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        let (code, _, _) = t
            .call(
                "GET",
                "/api/status",
                None,
                &[("authorization", "Bearer script-tokeN")],
            )
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // Behind TLS the session cookie is Secure.
        let (_, h, _) = t.login("admin", "hunter2hunter2").await;
        assert!(h[header::SET_COOKIE].to_str().unwrap().contains("; Secure"));
    }

    #[tokio::test]
    async fn setup_only_from_loopback_and_only_once() {
        let mut c = Config::default();
        c.web.bind = "0.0.0.0".parse().unwrap();
        c.web.token = Some("script-token".into());
        let t = setup(c, None);
        let good = "username=alice&password=longenough&confirm=longenough";
        // A network client is told to use multi passwd.
        let (code, _, body) = t.call("GET", "/setup", None, &[]).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(body.contains("multi passwd"));
        let (code, _, _) = t.call("POST", "/setup", Some(good.into()), &[FORM]).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        // A loopback peer with a foreign Host (DNS rebinding) is refused too.
        let lo = IpAddr::from([127, 0, 0, 1]);
        let (code, _, _) = t.call_from(lo, "GET", "/setup", None, &[]).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let local = [FORM, ("host", "localhost:8480")];
        let (code, _, body) = t.call_from(lo, "GET", "/setup", None, &local).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("action=\"/setup\""));
        let (code, _, body) = t
            .call_from(
                lo,
                "POST",
                "/setup",
                Some("username=alice&password=longenough&confirm=different1".into()),
                &local,
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("do not match"));
        let (code, _, _) = t
            .call_from(
                lo,
                "POST",
                "/setup",
                Some("username=alice&password=short&confirm=short".into()),
                &local,
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        let (code, _, _) = t
            .call_from(
                lo,
                "POST",
                "/setup",
                Some(good.into()),
                &[
                    FORM,
                    ("host", "localhost:8480"),
                    ("origin", "http://evil.example"),
                ],
            )
            .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, h, _) = t
            .call_from(lo, "POST", "/setup", Some(good.into()), &local)
            .await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        let cookie = cookie_of(&h);
        let saved = Config::load(&t.state.path).unwrap();
        assert_eq!(saved.web.username, "alice");
        let hash = saved.web.password_hash.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(auth::verify_password("longenough", &hash));
        let (code, _, _) = t
            .call("GET", "/api/status", None, &[("cookie", &cookie)])
            .await;
        assert_eq!(code, StatusCode::OK);
        // Disabled once a password exists.
        let (code, _, _) = t.call_from(lo, "GET", "/setup", None, &local).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let again = "username=mallory&password=otherpass1&confirm=otherpass1";
        let (code, _, _) = t
            .call_from(lo, "POST", "/setup", Some(again.into()), &local)
            .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(Config::load(&t.state.path).unwrap().web.username, "alice");
    }

    #[tokio::test]
    async fn no_password_loopback_gets_full_access_and_a_hint() {
        let t = setup(Config::default(), None);
        let (code, _, body) = t.call("GET", "/api/me", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(json(&body)["via"], "local");
        assert_eq!(json(&body)["password_set"], false);
        // An unknown peer is never treated as loopback.
        let (code, _, _) = t
            .call_from(
                IpAddr::from([192, 168, 1, 4]),
                "GET",
                "/api/status",
                None,
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_required_when_saving_a_public_bind() {
        let t = setup(Config::default(), None);
        let mut c = Config::default();
        c.web.bind = "0.0.0.0".parse().unwrap();
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&c).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json(&body)["issues"][0]["path"], "web.token");
        c.web.token = Some("abc".into());
        let (code, _, body) = t
            .call(
                "PUT",
                "/api/config",
                Some(serde_json::to_string(&c).unwrap()),
                &[],
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            json(&body)["report"]["server_restart"],
            serde_json::json!(["web.bind"])
        );
        let (_, _, body) = t.call("GET", "/api/status", None, &[]).await;
        assert_eq!(
            json(&body)["server_restart_pending"],
            serde_json::json!(["web.bind"])
        );
    }

    #[test]
    fn https_only_with_a_generated_certificate() {
        use std::io::Read;
        let mut c = Config::default();
        c.web.tls = multi_core::config::TlsMode::On;
        let t = setup(c.clone(), None);
        // Made on first use next to the config, reused afterwards.
        let made = crate::tls::load(&c.web, &t.state.path).unwrap();
        assert!(made.generated);
        assert_eq!(made.fingerprint.len(), 32 * 3 - 1);
        let again = crate::tls::load(&c.web, &t.state.path).unwrap();
        assert!(!again.generated);
        assert_eq!(again.fingerprint, made.fingerprint);
        let key = t.state.path.with_file_name(crate::tls::SELF_KEY);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = axum_server::Handle::new();
        let server = rt.spawn(run_server(
            listener,
            router(t.state.clone()),
            Some(made.config),
            handle.clone(),
        ));

        let pem = std::fs::read(&made.cert_path).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        use rustls::pki_types::pem::PemObject;
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
            roots.add(cert.unwrap()).unwrap();
        }
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let agent = ureq::AgentBuilder::new()
            .tls_config(Arc::new(client))
            .build();
        let url = format!("https://127.0.0.1:{port}/api/me");
        let mut resp = None;
        for _ in 0..50 {
            match agent.get(&url).call() {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let resp = resp.expect("HTTPS request failed");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_str(&resp.into_string().unwrap()).unwrap();
        assert_eq!(body["tls"], true);

        // Plain HTTP on the same port gets no HTTP answer.
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(b"GET /api/me HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        assert!(
            !buf.starts_with(b"HTTP/"),
            "{:?}",
            String::from_utf8_lossy(&buf)
        );

        handle.graceful_shutdown(Some(Duration::from_secs(1)));
        rt.block_on(server).unwrap().unwrap();
    }

    #[tokio::test]
    async fn page_is_served() {
        let t = setup(Config::default(), None);
        let (code, _, body) = t.call("GET", "/", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("app.js"));
        let (code, h, _) = t.call("GET", "/app.js", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(h["content-type"].to_str().unwrap().contains("javascript"));
    }
}
