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
