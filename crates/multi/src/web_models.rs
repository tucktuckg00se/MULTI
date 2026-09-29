//! Web API for models (M2-1): the catalogue with installed state, pulls in
//! the background (one at a time, the rest queued) with progress on
//! `/api/models/events`, verify, and remove. Mounted by `web.rs` behind its
//! guard (sign-in, `X-Multi: 1` on writes).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use multi_core::models::{AsrEngine, Format, Kind, Model, Origin, Registry};
use serde::Serialize;
use tokio::sync::broadcast;

use super::{ApiError, AppState};
use crate::models::{self as mm, Check, Phase};

/// Pull queue and progress, shared by the handlers and the pull thread.
pub struct Hub {
    root: PathBuf,
    /// User catalogue file; `None` = the process default.
    catalogue: Option<PathBuf>,
    st: Mutex<HubSt>,
    events: broadcast::Sender<ModelEvent>,
}

#[derive(Default)]
struct HubSt {
    queue: VecDeque<String>,
    active: Option<Progress>,
    /// Last failure per model, until it is pulled again.
    errors: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    id: String,
    phase: Phase,
    done: u64,
    total: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ModelEvent {
    Queued { id: String },
    Progress(Progress),
    Done { id: String },
    Failed { id: String, error: String },
    Removed { id: String },
}

impl Hub {
    pub fn new(root: PathBuf, catalogue: Option<PathBuf>) -> Self {
        Self {
            root,
            catalogue,
            st: Mutex::default(),
            events: broadcast::channel(256).0,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubSt> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn catalogue(&self) -> Result<(Registry, Vec<String>), ApiError> {
        mm::load_catalogue(self.catalogue.as_deref())
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
    }

    fn send(&self, ev: ModelEvent) {
        let _ = self.events.send(ev);
    }

    /// Runs queued pulls until the queue is empty (on its own thread).
    fn work(self: Arc<Self>) {
        loop {
            let id = {
                let mut st = self.lock();
                let Some(id) = st.queue.pop_front() else {
                    st.active = None;
                    return;
                };
                st.errors.retain(|(e, _)| *e != id);
                st.active = Some(Progress {
                    id: id.clone(),
                    phase: Phase::Download,
                    done: 0,
                    total: 0,
                });
                id
            };
            let result = self.catalogue().map_err(|e| e.1).and_then(|(reg, _)| {
                let m = reg
                    .get(&id)
                    .ok_or_else(|| format!("{id} is no longer in the catalogue"))?;
                self.pull(m).map_err(|e| format!("{e:#}"))
            });
            let ev = match result {
                Ok(()) => ModelEvent::Done { id },
                Err(error) => {
                    self.lock().errors.push((id.clone(), error.clone()));
                    ModelEvent::Failed { id, error }
                }
            };
            // `active` stays set until the queue is empty, so a new pull
            // never starts a second thread.
            self.send(ev);
        }
    }

    fn pull(&self, m: &Model) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let last = Mutex::new((Phase::Download, u64::MAX));
        let report = |phase: Phase, done: u64, total: u64| {
            // At most ~200 events per phase.
            let step = (total / 200).max(1 << 16);
            let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
            let due = l.0 != phase || l.1 == u64::MAX || done >= l.1 + step || done == total;
            if !due {
                return;
            }
            *l = (phase, done);
            let p = Progress {
                id: m.id.clone(),
                phase,
                done,
                total,
            };
            self.lock().active = Some(p.clone());
            self.send(ModelEvent::Progress(p));
        };
        mm::pull_one_with(m, &self.root, &report)
    }

    fn busy_with(&self, id: &str) -> bool {
        let st = self.lock();
        st.active.as_ref().is_some_and(|a| a.id == id) || st.queue.iter().any(|q| q == id)
    }
}

// ---------------------------------------------------------------- list

#[derive(Serialize)]
struct LanguageOut {
    code: String,
    name: String,
    script: String,
    formats: Vec<Format>,
    note: Option<&'static str>,
}

#[derive(Serialize)]
struct ModelOut {
    id: String,
    kind: Kind,
    origin: Origin,
    default: bool,
    status: &'static str,
    /// Bytes on disk when installed.
    size: u64,
    disk_mb: u32,
    vram_mb: u32,
    licence: String,
    attribution: String,
    /// Spoken languages (ASR).
    languages: Vec<String>,
    /// ASR (M2-5): engine, chunk (Nemotron) or pass interval (Whisper),
    /// typical lag, one-line note, the CPU variant used when this one is
    /// not installed.
    engine: Option<AsrEngine>,
    chunk_ms: Option<u32>,
    lag_ms: Option<u32>,
    note: Option<String>,
    cpu_variant: Option<String>,
    source: Option<String>,
    targets: Vec<String>,
    /// Caption formats that can carry the target language(s) (MT).
    formats: Vec<Format>,
    in_use: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct ListOut {
    models_dir: PathBuf,
    catalogue: PathBuf,
    warnings: Vec<String>,
    running: bool,
    active: Option<Progress>,
    queue: Vec<String>,
    languages: Vec<LanguageOut>,
    models: Vec<ModelOut>,
}

/// Ids in use by the running pipeline (empty when stopped).
fn in_use(state: &AppState, reg: &Registry) -> Vec<String> {
    if !state.service.is_active() {
        return Vec::new();
    }
    mm::in_use(reg, &state.service.config(), &state.models.root)
}

pub async fn list(State(state): State<AppState>) -> Response {
    let hub = state.models.clone();
    let st2 = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        let (reg, warnings) = hub.catalogue()?;
        let used = in_use(&st2, &reg);
        let (active, queue, errors) = {
            let st = hub.lock();
            (
                st.active.clone(),
                st.queue.iter().cloned().collect(),
                st.errors.clone(),
            )
        };
        let languages = reg
            .languages
            .iter()
            .map(|l| LanguageOut {
                code: l.code.clone(),
                name: l.name.clone(),
                script: l.script.clone(),
                formats: l.formats(),
                note: l.note(),
            })
            .collect();
        let models = reg
            .models
            .iter()
            .map(|m| {
                let status = mm::status(m, &hub.root);
                let formats = m
                    .targets
                    .iter()
                    .map(|t| {
                        reg.language(t)
                            .map_or(vec![Format::WebVtt], |l| l.formats())
                    })
                    .reduce(|a, b| a.into_iter().filter(|f| b.contains(f)).collect())
                    .unwrap_or_default();
                ModelOut {
                    id: m.id.clone(),
                    kind: m.kind,
                    origin: m.origin,
                    default: m.default,
                    status,
                    size: if status == "missing" {
                        0
                    } else {
                        mm::size_on_disk(m, &hub.root)
                    },
                    disk_mb: m.disk_mb,
                    vram_mb: m.vram_mb,
                    licence: m.licence.clone(),
                    attribution: m.attribution.clone(),
                    languages: m.languages.clone(),
                    engine: m.asr_engine(),
                    chunk_ms: m.chunk_ms,
                    lag_ms: m.lag_ms,
                    note: m.note.clone(),
                    cpu_variant: m.cpu_variant.clone(),
                    source: m.source.clone(),
                    targets: m.targets.clone(),
                    formats,
                    in_use: used.contains(&m.id),
                    error: errors.iter().find(|(e, _)| *e == m.id).map(|e| e.1.clone()),
                }
            })
            .collect();
        Ok::<_, ApiError>(ListOut {
            models_dir: hub.root.clone(),
            catalogue: hub
                .catalogue
                .clone()
                .unwrap_or_else(|| mm::catalogue_path().0),
            warnings,
            running: st2.service.is_active(),
            active,
            queue,
            languages,
            models,
        })
    })
    .await;
    match res {
        Ok(Ok(out)) => Json(out).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------- actions

fn not_found(id: &str) -> Response {
    ApiError(
        StatusCode::NOT_FOUND,
        format!("unknown model `{id}`; it is not in the catalogue"),
    )
    .into_response()
}

pub async fn pull(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let hub = state.models.clone();
    let reg = match hub.catalogue() {
        Ok((r, _)) => r,
        Err(e) => return e.into_response(),
    };
    let Some(m) = reg.get(&id) else {
        return not_found(&id);
    };
    if m.installed(&hub.root) {
        return ApiError(StatusCode::CONFLICT, format!("{id} is already installed"))
            .into_response();
    }
    let start = {
        let mut st = hub.lock();
        if st.active.as_ref().is_some_and(|a| a.id == id) || st.queue.contains(&id) {
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "queued": id })),
            )
                .into_response();
        }
        st.queue.push_back(id.clone());
        let idle = st.active.is_none() && st.queue.len() == 1;
        if idle {
            // Claim the worker slot before the thread starts.
            st.active = Some(Progress {
                id: id.clone(),
                phase: Phase::Download,
                done: 0,
                total: 0,
            });
        }
        idle
    };
    hub.send(ModelEvent::Queued { id: id.clone() });
    if start {
        let h = hub.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("model-pull".into())
            .spawn(move || h.work())
        {
            let mut st = hub.lock();
            st.queue.clear();
            st.active = None;
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "queued": id })),
    )
        .into_response()
}

#[derive(Serialize)]
struct VerifyOut {
    id: String,
    result: &'static str,
    problems: Vec<String>,
}

pub async fn verify(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let hub = state.models.clone();
    if hub.busy_with(&id) {
        return ApiError(StatusCode::CONFLICT, format!("{id} is being pulled")).into_response();
    }
    let res = tokio::task::spawn_blocking(move || {
        let (reg, _) = hub.catalogue()?;
        let m = reg
            .get(&id)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown model `{id}`")))?;
        let check = mm::check_model(m, &hub.root)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        let (result, problems) = match check {
            Check::Ok => ("ok", Vec::new()),
            Check::Missing => ("missing", Vec::new()),
            Check::Unverified(why) => ("unverified", vec![why]),
            Check::Failed(p) => ("failed", p),
        };
        Ok::<_, ApiError>(VerifyOut {
            id,
            result,
            problems,
        })
    })
    .await;
    match res {
        Ok(Ok(out)) => Json(out).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn remove(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let hub = state.models.clone();
    if hub.busy_with(&id) {
        return ApiError(StatusCode::CONFLICT, format!("{id} is being pulled")).into_response();
    }
    let res = tokio::task::spawn_blocking(move || {
        let (reg, _) = hub.catalogue()?;
        if reg.get(&id).is_none() {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown model `{id}`"),
            ));
        }
        if in_use(&state, &reg).contains(&id) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("{id} is in use by the running pipeline; stop the pipeline (or change the languages) first"),
            ));
        }
        mm::remove(&reg, &id, &hub.root)
            .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
        hub.send(ModelEvent::Removed { id: id.clone() });
        Ok(id)
    })
    .await;
    match res {
        Ok(Ok(id)) => Json(serde_json::json!({ "removed": id })).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.models.events.subscribe();
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

/// Arc alias so `web.rs` names only one type.
pub type SharedHub = Arc<Hub>;
