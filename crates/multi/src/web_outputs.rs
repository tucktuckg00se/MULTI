//! Per-output control: `POST /api/outputs/{index}/start` and `/stop`.
//!
//! Sets `outputs[index].enabled`, saves the config atomically (like
//! `PUT /api/config`) and applies it live: only that output starts or
//! stops. A child module of `web`, behind the same auth and `X-Multi` guard.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use multi_core::Config;
use tracing::info;

use super::{ApiError, AppState, Saved, mask, write_atomic};
use crate::service::ApplyReport;

pub(super) async fn start(State(state): State<AppState>, Path(index): Path<usize>) -> Response {
    set_enabled(state, index, true).await
}

pub(super) async fn stop(State(state): State<AppState>, Path(index): Path<usize>) -> Response {
    set_enabled(state, index, false).await
}

async fn set_enabled(state: AppState, index: usize, on: bool) -> Response {
    let st = state.clone();
    // The store stays locked from read to apply, so two clicks cannot
    // interleave their save and live change.
    let done = tokio::task::spawn_blocking(move || -> Result<(ApplyReport, Config), ApiError> {
        let mut store = st.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut new = store.clone();
        let Some(o) = new.outputs.get_mut(index) else {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("no output {index}"),
            ));
        };
        o.enabled = on;
        let text = new
            .to_toml()
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        write_atomic(&st.path, &text).map_err(|e| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot save {}: {e}", st.path.display()),
            )
        })?;
        *store = new.clone();
        let report = st.service.apply(new.clone());
        Ok((report, new))
    })
    .await;
    match done {
        Ok(Ok((report, config))) => {
            info!(
                index,
                enabled = on,
                "output {}",
                if on { "started" } else { "stopped" }
            );
            Json(Saved {
                report,
                config: mask(&config),
            })
            .into_response()
        }
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
