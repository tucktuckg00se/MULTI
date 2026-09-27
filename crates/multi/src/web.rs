//! `multi serve`: the web GUI and its JSON API, on top of [`Service`].
//!
//! Routes: `/` (the page), `/api/config` (GET/PUT), `/api/config/default`,
//! `/api/start`, `/api/stop`, `/api/status`, `/api/events` (SSE), `/login`.
//!
//! Security: on a loopback bind no token is needed, but the `Host` header must
//! name a loopback host (DNS rebinding). On any other bind a token is required
//! (`MULTI_WEB_TOKEN`, else `web.token`) as `Authorization: Bearer <token>` or
//! the cookie set by `/login`. Every POST/PUT under `/api` must carry
//! `X-Multi: 1`, which a cross-site form cannot send. Secrets (stream keys,
//! passphrases, the token) are masked in every response; a masked value sent
//! back keeps the stored one.

use std::convert::Infallible;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::{Form, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt;
use multi_core::Config;
use multi_core::config::Issue;
use multi_media::url::redact;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::service::{
    ApplyReport, Effect, Service, ServiceStatus, Workers, changed_paths, effect_of,
};

/// Shown instead of the token.
pub const MASK: &str = "********";
pub const TOKEN_ENV: &str = "MULTI_WEB_TOKEN";
const COOKIE: &str = "multi_token";

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const LOGIN_HTML: &str = include_str!("../web/login.html");

/// Shared state of the web server.
#[derive(Clone)]
pub struct AppState {
    pub service: Service,
    store: Arc<Mutex<Config>>,
    path: Arc<PathBuf>,
    /// Address the server was started on (changes need a server restart).
    web: (IpAddr, u16),
    /// Token from the environment; wins over `web.token`.
    env_token: Option<String>,
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
        let web = (config.web.bind, config.web.port);
        Self {
            service,
            store: Arc::new(Mutex::new(config)),
            path: Arc::new(path),
            web,
            env_token: env_token.filter(|t| !t.is_empty()),
            shutdown,
        }
    }

    fn config(&self) -> Config {
        self.store
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn auth_required(&self) -> bool {
        !self.web.0.is_loopback()
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
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/default", get(default_config))
        .route("/api/start", axum::routing::post(start))
        .route("/api/stop", axum::routing::post(stop))
        .route("/api/status", get(status))
        .route("/api/events", get(events))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

// ---------------------------------------------------------------- security

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = state.token() else {
        return false; // fail closed
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if bearer.is_some_and(|b| eq_ct(b.trim().as_bytes(), token.as_bytes())) {
        return true;
    }
    let want = hex(&token);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && eq_ct(v.as_bytes(), want.as_bytes()))
}

async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let api = path.starts_with("/api/");
    if api
        && matches!(*req.method(), Method::POST | Method::PUT)
        && req.headers().get("x-multi").is_none_or(|v| v != "1")
    {
        return ApiError(StatusCode::FORBIDDEN, "missing X-Multi: 1 header".into()).into_response();
    }
    if !state.auth_required() {
        if !loopback_host(req.headers()) {
            return ApiError(StatusCode::FORBIDDEN, "host not allowed".into()).into_response();
        }
        return next.run(req).await;
    }
    let public = matches!(path.as_str(), "/login" | "/app.css");
    if public || authorized(&state, req.headers()) {
        return next.run(req).await;
    }
    if api {
        ApiError(StatusCode::UNAUTHORIZED, "token required".into()).into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_page() -> Html<String> {
    Html(LOGIN_HTML.replace("{{error}}", ""))
}

async fn login(State(state): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let ok = state
        .token()
        .is_some_and(|t| eq_ct(f.token.trim().as_bytes(), t.as_bytes()));
    if !ok {
        warn!("web login failed");
        let page = LOGIN_HTML.replace(
            "{{error}}",
            "<p class=\"err\" role=\"alert\">That token is not right.</p>",
        );
        return (StatusCode::UNAUTHORIZED, Html(page)).into_response();
    }
    let cookie = format!(
        "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
        hex(f.token.trim())
    );
    let mut resp = Redirect::to("/").into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
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

/// The config with stream keys, passphrases and the token hidden.
pub fn mask(c: &Config) -> Config {
    let mut m = c.clone();
    m.input.url = redact(&m.input.url);
    for o in &mut m.outputs {
        o.url = redact(&o.url);
    }
    if m.web.token.as_deref().is_some_and(|t| !t.is_empty()) {
        m.web.token = Some(MASK.into());
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
    if (!c.web.bind.is_loopback() || state.auth_required()) && !has_token {
        v.push(Issue {
            path: "web.token".into(),
            message: format!(
                "a token is required when the GUI is reachable from other machines (or set {TOKEN_ENV})"
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
    let stored = state.config();
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
    StatusOut {
        status,
        config_issues,
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
    if !config.web.bind.is_loopback() && !has_token {
        bail!(
            "web.bind {} is reachable from other machines: set {TOKEN_ENV} or web.token",
            config.web.bind
        );
    }
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
    let state = AppState::new(service.clone(), config, path.to_path_buf(), env_token, rx);
    let served = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        let shown = if addr.ip().is_unspecified() {
            format!("http://<this-host>:{}/", addr.port())
        } else {
            format!("http://{addr}/")
        };
        info!("web GUI at {shown}");
        axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown_signal(tx))
            .await
            .context("web server failed")
    });
    info!("shutting down the pipeline");
    let stopped = service.stop();
    served.and(stopped)
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
        _tx: watch::Sender<bool>,
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
        let service = Service::new(config.clone(), Workers::default());
        let state = AppState::new(service, config, path, env_token.map(String::from), rx);
        T {
            app: router(state.clone()),
            state,
            _dir: TempDir(dir),
            host,
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
            let mut b = Request::builder()
                .method(method)
                .uri(uri)
                .header("host", self.host)
                .header("x-multi", "1")
                .header("content-type", "application/json");
            if let Some(h) = b.headers_mut() {
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
            Output {
                url: "rtmp://a.rtmp.youtube.com/live2/abcd-efgh-key".into(),
            },
            Output {
                url: "srt://h:1?mode=caller&passphrase=outsecret".into(),
            },
        ];
        c.web.token = Some("tok-123".into());
        let t = setup(c.clone(), None);
        let (_, _, body) = t.call("GET", "/api/config", None, &[]).await;
        for secret in ["inputsecret", "abcd-efgh-key", "outsecret", "tok-123"] {
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
        // Login page sets a cookie that works.
        let (code, _, _) = t
            .call(
                "POST",
                "/login",
                Some("token=wrong".into()),
                &[("content-type", "application/x-www-form-urlencoded")],
            )
            .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, h, _) = t
            .call(
                "POST",
                "/login",
                Some("token=from-env".into()),
                &[("content-type", "application/x-www-form-urlencoded")],
            )
            .await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        let cookie = h[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let (code, _, body) = t.call("GET", "/", None, &[("cookie", &cookie)]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("<title>"));
        let (code, _, _) = t.call("GET", "/login", None, &[]).await;
        assert_eq!(code, StatusCode::OK);
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
