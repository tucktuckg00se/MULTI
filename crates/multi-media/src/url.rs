//! Input and output URLs: which kind of endpoint they are, how GStreamer is
//! given them, and how they are shown in logs without secrets.

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// `srt://host:port?mode=caller|listener&...`
    Srt,
    /// `udp://host:port` (unicast or multicast group), raw MPEG-TS.
    Udp,
    /// `rtp://host:port`: MPEG-TS in RTP (payload type 33), unicast or multicast.
    Rtp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Srt,
    Udp,
    /// `rtmp://` or `rtmps://`: FLV (H.264 + AAC) to an RTMP server.
    Rtmp,
    /// `hls://<name>?segment_s=2&window=6`: HLS with WebVTT subtitles,
    /// written to a MULTI-owned directory and served by the web server.
    Hls,
}

/// Settings of an `hls://` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsParams {
    pub name: String,
    /// Target segment length, seconds (segments are cut on keyframes).
    pub segment_s: u32,
    /// Segments listed in the live playlists.
    pub window: u32,
}

/// Parses `hls://<name>[?segment_s=N&window=N]`.
pub fn hls_params(url: &str) -> Result<HlsParams> {
    let Some(rest) = url.strip_prefix("hls://") else {
        bail!("not an hls:// URL");
    };
    let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !multi_core::config::hls_name_ok(name) {
        bail!("bad HLS output name {name:?}: use 1–32 of a-z, 0-9, _ and -");
    }
    let mut p = HlsParams {
        name: name.to_string(),
        segment_s: 2,
        window: 6,
    };
    for kv in query.split('&').filter(|kv| !kv.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let n: u32 = v.parse().unwrap_or(0);
        match k {
            "segment_s" if (1..=10).contains(&n) => p.segment_s = n,
            "window" if (3..=30).contains(&n) => p.window = n,
            _ => bail!("bad HLS option {kv}"),
        }
    }
    Ok(p)
}

fn scheme(url: &str) -> &str {
    url.split_once("://").map_or("", |(s, _)| s)
}

pub fn input_kind(url: &str) -> Result<InputKind> {
    Ok(match scheme(url) {
        "srt" => InputKind::Srt,
        "udp" => InputKind::Udp,
        "rtp" => InputKind::Rtp,
        _ => bail!("unsupported input URL {}", redact(url)),
    })
}

pub fn output_kind(url: &str) -> Result<OutputKind> {
    Ok(match scheme(url) {
        "srt" => OutputKind::Srt,
        "udp" => OutputKind::Udp,
        "rtmp" | "rtmps" => OutputKind::Rtmp,
        "hls" => {
            hls_params(url)?;
            OutputKind::Hls
        }
        _ => bail!("unsupported output URL {}", redact(url)),
    })
}

/// The URI handed to `srtsrc`/`srtsink`: adds `latency=<ms>` unless the URL
/// sets its own.
pub fn srt_uri(url: &str, latency_ms: u32) -> String {
    let has_latency = url
        .split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|kv| kv.starts_with("latency=")));
    if has_latency {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&latency={latency_ms}")
    } else {
        format!("{url}?latency={latency_ms}")
    }
}

/// `udp://host:port` for `udpsrc`/`udpsink`. Query parameters (e.g. FFmpeg's
/// `pkt_size`) mean nothing to GStreamer and are dropped.
pub fn udp_uri(url: &str) -> String {
    let base = url.split_once('?').map_or(url, |(b, _)| b);
    match base.split_once("://") {
        Some((_, rest)) => format!("udp://{rest}"),
        None => base.to_string(),
    }
}

/// The URL with secrets removed, for logs and status: SRT `passphrase` and
/// `streamid` values, and the last path segment (stream key) of RTMP URLs.
pub fn redact(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let mut out = match scheme(base) {
        "rtmp" | "rtmps" => {
            let rest = base.split_once("://").map_or("", |(_, r)| r);
            // host[:port]/app/key -> host[:port]/app/***
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() > 2 {
                let keep = parts[..parts.len() - 1].join("/");
                format!("{}://{keep}/***", scheme(base))
            } else {
                base.to_string()
            }
        }
        _ => base.to_string(),
    };
    if let Some(q) = query {
        let q: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) if matches!(k, "passphrase" | "streamid" | "key") => {
                    format!("{k}=***")
                }
                _ => kv.to_string(),
            })
            .collect();
        out.push('?');
        out.push_str(&q.join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(input_kind("srt://1.2.3.4:9000").ok(), Some(InputKind::Srt));
        assert_eq!(
            input_kind("udp://239.1.1.1:5000").ok(),
            Some(InputKind::Udp)
        );
        assert_eq!(input_kind("rtp://0.0.0.0:5004").ok(), Some(InputKind::Rtp));
        assert!(input_kind("http://x").is_err());
        assert_eq!(output_kind("rtmps://a/b/c").ok(), Some(OutputKind::Rtmp));
        assert!(output_kind("rtp://a:1").is_err());
        assert_eq!(output_kind("hls://web").ok(), Some(OutputKind::Hls));
        assert!(output_kind("hls://../etc").is_err());
    }

    #[test]
    fn hls_options() -> Result<()> {
        let p = hls_params("hls://web?segment_s=4&window=10")?;
        assert_eq!((p.name.as_str(), p.segment_s, p.window), ("web", 4, 10));
        let p = hls_params("hls://web")?;
        assert_eq!((p.segment_s, p.window), (2, 6));
        assert!(hls_params("hls://web?window=1").is_err());
        assert!(hls_params("hls://web?x=1").is_err());
        Ok(())
    }

    #[test]
    fn srt_latency_added_once() {
        assert_eq!(srt_uri("srt://h:1", 120), "srt://h:1?latency=120");
        assert_eq!(
            srt_uri("srt://h:1?mode=listener", 80),
            "srt://h:1?mode=listener&latency=80"
        );
        assert_eq!(
            srt_uri("srt://h:1?latency=500&mode=caller", 80),
            "srt://h:1?latency=500&mode=caller"
        );
    }

    #[test]
    fn udp_query_dropped() {
        assert_eq!(
            udp_uri("udp://127.0.0.1:9?pkt_size=1316"),
            "udp://127.0.0.1:9"
        );
        assert_eq!(udp_uri("rtp://239.0.0.1:5004"), "udp://239.0.0.1:5004");
    }

    #[test]
    fn secrets_redacted() {
        assert_eq!(
            redact("rtmp://a.rtmp.youtube.com/live2/abcd-efgh"),
            "rtmp://a.rtmp.youtube.com/live2/***"
        );
        assert_eq!(redact("rtmp://host/app"), "rtmp://host/app");
        assert_eq!(
            redact("srt://h:1?mode=caller&passphrase=secret123&latency=80"),
            "srt://h:1?mode=caller&passphrase=***&latency=80"
        );
    }
}
