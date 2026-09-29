//! HLS web output (M2-3): the viewer page and the playlists and segments
//! that `hls://<name>` outputs write (see `multi_media::hls`).
//!
//! - `GET /watch/<name>`: `<video>` with hls.js (vendored, Apache-2.0) and a
//!   caption language picker.
//! - `GET /hls/<name>/<file>`: `master.m3u8`, `video.m3u8`, `sub_<lang>.m3u8`
//!   and the `.ts`/`.vtt` segments, for configured HLS outputs only.
//!
//! Access: an output with `public = true` makes `/watch/<name>` and
//! `/hls/<name>/…` open to anyone who can reach the server, with no sign-in;
//! nothing else becomes public. Otherwise the normal guard applies.

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use multi_core::Config;

use super::AppState;

const WATCH_HTML: &str = include_str!("../web/watch.html");
const WATCH_JS: &str = include_str!("../web/watch.js");
/// hls.js 1.7.3, Apache-2.0 (`web/hls.js-LICENSE.txt`).
const HLS_JS: &str = include_str!("../web/hls.min.js");

/// The HLS output named `name` in `c`, if any: whether it is public.
fn hls_output(c: &Config, name: &str) -> Option<bool> {
    c.outputs
        .iter()
        .find(|o| {
            o.url
                .strip_prefix("hls://")
                .map(|r| r.split('?').next().unwrap_or(r))
                == Some(name)
        })
        .map(|o| o.public)
}

/// Whether `path` is open without sign-in: the viewer's scripts always, the
/// page and the stream of a public HLS output.
pub(super) fn public(c: &Config, path: &str) -> bool {
    if matches!(path, "/watch.js" | "/hls.min.js") {
        return true;
    }
    let name = if let Some(n) = path.strip_prefix("/watch/") {
        n
    } else if let Some(rest) = path.strip_prefix("/hls/") {
        rest.split('/').next().unwrap_or("")
    } else {
        return false;
    };
    hls_output(c, name) == Some(true)
}

pub(super) async fn watch(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if hls_output(&state.config(), &name).is_none() {
        return (StatusCode::NOT_FOUND, "no such HLS output").into_response();
    }
    Html(WATCH_HTML).into_response()
}

pub(super) async fn watch_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        WATCH_JS,
    )
}

pub(super) async fn hls_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        HLS_JS,
    )
}

/// Content type and cache policy of a served file, or `None` if the name
/// is not one an HLS output writes.
fn file_kind(file: &str) -> Option<(&'static str, &'static str)> {
    let ok = !file.starts_with('.')
        && file.len() <= 64
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !ok {
        return None;
    }
    match file.rsplit_once('.')?.1 {
        "m3u8" => Some(("application/vnd.apple.mpegurl", "no-cache")),
        "ts" => Some(("video/mp2t", "max-age=60")),
        "vtt" => Some(("text/vtt; charset=utf-8", "max-age=60")),
        _ => None,
    }
}

pub(super) async fn file(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "not found").into_response();
    let Some((ctype, cache)) = file_kind(&file) else {
        return not_found();
    };
    if !multi_core::config::hls_name_ok(&name) || hls_output(&state.config(), &name).is_none() {
        return not_found();
    }
    let path = multi_media::hls::output_dir(&name).join(&file);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(data)) => (
            [
                (header::CONTENT_TYPE, ctype),
                (header::CACHE_CONTROL, cache),
            ],
            data,
        )
            .into_response(),
        _ => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multi_core::config::Output;

    #[test]
    fn public_paths_and_file_names() {
        let mut c = Config::default();
        c.outputs.push(Output {
            public: true,
            ..Output::new("hls://open?segment_s=2")
        });
        c.outputs.push(Output::new("hls://closed"));
        assert!(public(&c, "/watch/open"));
        assert!(public(&c, "/hls/open/master.m3u8"));
        assert!(public(&c, "/hls.min.js"));
        assert!(!public(&c, "/watch/closed"));
        assert!(!public(&c, "/hls/closed/master.m3u8"));
        assert!(!public(&c, "/hls/nope/master.m3u8"));
        assert!(!public(&c, "/api/status"));
        assert!(file_kind("sub_ar_00012.vtt").is_some());
        assert!(file_kind("v00001.ts").is_some());
        for bad in ["../x.ts", ".m3u8", "a/b.ts", "x.txt", "x"] {
            assert!(file_kind(bad).is_none(), "{bad}");
        }
    }
}
