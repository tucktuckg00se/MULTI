//! HLS web output: WebVTT subtitle renditions and playlists, next to the
//! A/V-only HLS that `hlssink2` writes (M2-3, per R1: no GStreamer HLS
//! element carries WebVTT).
//!
//! ```text
//! CaptionHandle -> TextHub (every WebVTT language, after cleaning and the word filter)
//! output ES (H.264/H.265 + AAC) -> hlssink2 -> v%05d.ts + video.m3u8
//! HlsWriter (on each output poll): video.m3u8 -> for each new A/V segment,
//!   one sub_<lang>_<seq>.vtt per language over the same PTS span,
//!   sub_<lang>.m3u8, and master.m3u8
//! ```
//!
//! Timing: a caption's display time (arrival + `captions.offset_ms`) is
//! mapped to the output timeline through the last video buffer that went to
//! the HLS pipeline, then to MPEG-TS PTS (`hlssink2`'s muxer adds
//! [`TS_BASE`]). Each VTT segment carries `X-TIMESTAMP-MAP` with the first
//! video PTS read from its A/V segment, so players line cues up with video.
//! A cue lasts until the next line or `captions.clear_after_ms`; a cue that
//! crosses a segment boundary is written, clipped, into both segments.
//!
//! All of this runs on the media control thread's output poll, never on a
//! streaming thread, and failures only log: HLS never disturbs other outputs.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use multi_core::config::Language;
use tracing::{info, warn};

use crate::url::HlsParams;

/// `mpegtsmux` writes PTS = running time + 3600 s (measured with
/// `hlssink2`, GStreamer 1.28).
pub const TS_BASE: u64 = 3600 * 90_000;
const WRAP: u64 = 1 << 33;
/// Lines kept for HLS writers that have not read them yet.
const HUB_CAP: usize = 4096;
/// Bytes of an A/V segment read to find its first video PTS.
const PTS_PROBE_BYTES: u64 = 256 * 1024;

/// Nanoseconds on a process-wide monotonic clock.
pub(crate) fn mono_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Where HLS outputs write: `$MULTI_HLS_DIR`, else
/// `$XDG_RUNTIME_DIR/multi/hls`, else `<tmp>/multi-hls`.
pub fn root_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MULTI_HLS_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("multi/hls"),
        None => std::env::temp_dir().join("multi-hls"),
    }
}

/// The directory of HLS output `name` (a name checked by
/// [`multi_core::config::hls_name_ok`]).
pub fn output_dir(name: &str) -> PathBuf {
    root_dir().join(name)
}

/// The language's own name, for the player's menu.
pub fn endonym(code: &str) -> &str {
    match code {
        "ar" => "العربية",
        "bg" => "Български",
        "ca" => "Català",
        "cs" => "Čeština",
        "da" => "Dansk",
        "de" => "Deutsch",
        "el" => "Ελληνικά",
        "en" => "English",
        "es" => "Español",
        "fa" => "فارسی",
        "fi" => "Suomi",
        "fr" => "Français",
        "he" => "עברית",
        "hi" => "हिन्दी",
        "hu" => "Magyar",
        "id" => "Bahasa Indonesia",
        "it" => "Italiano",
        "ja" => "日本語",
        "ko" => "한국어",
        "nl" => "Nederlands",
        "no" => "Norsk",
        "pl" => "Polski",
        "pt" => "Português",
        "ro" => "Română",
        "ru" => "Русский",
        "sk" => "Slovenčina",
        "sv" => "Svenska",
        "th" => "ไทย",
        "tr" => "Türkçe",
        "uk" => "Українська",
        "vi" => "Tiếng Việt",
        "zh" => "中文",
        other => other,
    }
}

/// One caption line as the lanes get it.
#[derive(Clone, Debug)]
pub(crate) struct HubLine {
    seq: u64,
    lang: String,
    text: String,
    new_row: bool,
    /// Display time on [`mono_ns`].
    due_ns: u64,
}

#[derive(Default)]
struct HubState {
    lines: VecDeque<HubLine>,
    next: u64,
}

/// Caption lines for HLS writers: a bounded log each writer reads from its
/// own cursor. Pushing never blocks for long (one short lock).
#[derive(Default)]
pub struct TextHub {
    st: Mutex<HubState>,
}

impl TextHub {
    pub(crate) fn push(&self, lang: &str, text: &str, new_row: bool, due_ns: u64) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.next;
        st.next += 1;
        st.lines.push_back(HubLine {
            seq,
            lang: lang.to_string(),
            text: text.to_string(),
            new_row,
            due_ns,
        });
        while st.lines.len() > HUB_CAP {
            st.lines.pop_front();
        }
    }

    fn cursor(&self) -> u64 {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).next
    }

    /// Lines from `cursor` on, and the next cursor.
    fn since(&self, cursor: u64) -> (Vec<HubLine>, u64) {
        let st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let lines = st
            .lines
            .iter()
            .filter(|l| l.seq >= cursor)
            .cloned()
            .collect();
        (lines, st.next)
    }
}

/// A WebVTT language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VttLang {
    pub code: String,
    pub source: bool,
}

/// What HLS writers share: the text log and the caption settings.
pub struct HlsCtx {
    pub hub: Arc<TextHub>,
    /// Source language first.
    pub langs: Vec<VttLang>,
    pub clear_after_ms: u32,
    pub rows: usize,
    pub max_chars: usize,
}

impl HlsCtx {
    pub fn new(langs: &[Language], c: &multi_core::config::Captions) -> Self {
        let mut langs: Vec<VttLang> = langs
            .iter()
            .filter(|l| l.webvtt)
            .map(|l| VttLang {
                code: l.code.clone(),
                source: l.source,
            })
            .collect();
        langs.sort_by_key(|l| !l.source);
        Self {
            hub: Arc::new(TextHub::default()),
            langs,
            clear_after_ms: c.clear_after_ms.max(500),
            rows: usize::from(c.rows.clamp(1, 3)),
            max_chars: usize::from(c.max_chars_per_line.max(20)),
        }
    }

    /// No languages (unit tests of the output set).
    pub fn empty() -> Self {
        Self {
            hub: Arc::new(TextHub::default()),
            langs: Vec::new(),
            clear_after_ms: 4000,
            rows: 2,
            max_chars: 32,
        }
    }
}

// ---------------------------------------------------------------- WebVTT

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cue {
    /// 90 kHz, unwrapped MPEG-TS time.
    pub start: u64,
    pub end: u64,
    pub text: String,
}

/// The cues of one language: the rows on screen, the cue showing now, and
/// finished cues not yet written.
#[derive(Default)]
pub(crate) struct CueTrack {
    rows: Vec<String>,
    open: Option<(u64, String)>,
    closed: VecDeque<Cue>,
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}' | '\u{ff00}'..='\u{ffef}')
}

impl CueTrack {
    /// Ends the showing cue if it has been up for `hold` by `now`.
    pub fn expire(&mut self, now: u64, hold: u64) {
        if let Some((s, _)) = &self.open
            && s + hold <= now
            && let Some((s, text)) = self.open.take()
        {
            self.closed.push_back(Cue {
                start: s,
                end: s + hold,
                text,
            });
            self.rows.clear();
        }
    }

    /// A line shown at `ts`.
    pub fn line(&mut self, ts: u64, text: &str, new_row: bool, ctx: &HlsCtx) {
        let hold = u64::from(ctx.clear_after_ms) * 90;
        self.expire(ts, hold);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let mut ts = ts;
        if let Some((s, old)) = self.open.take() {
            ts = ts.max(s);
            if ts > s {
                self.closed.push_back(Cue {
                    start: s,
                    end: ts,
                    text: old,
                });
            }
        }
        match self.rows.last_mut() {
            Some(r)
                if !new_row && r.chars().count() + 1 + text.chars().count() <= ctx.max_chars =>
            {
                let join = !(r.chars().last().is_some_and(is_cjk)
                    && text.chars().next().is_some_and(is_cjk));
                if join {
                    r.push(' ');
                }
                r.push_str(&text);
            }
            _ => self.rows.push(text),
        }
        while self.rows.len() > ctx.rows {
            self.rows.remove(0);
        }
        self.open = Some((ts, self.rows.join("\n")));
    }

    /// Cues overlapping `[s, e)`, clipped to it.
    pub fn cues_in(&self, s: u64, e: u64, hold: u64) -> Vec<Cue> {
        let open = self.open.as_ref().map(|(st, t)| Cue {
            start: *st,
            end: st + hold,
            text: t.clone(),
        });
        self.closed
            .iter()
            .cloned()
            .chain(open)
            .filter(|c| c.end > s && c.start < e)
            .map(|c| Cue {
                start: c.start.max(s),
                end: c.end.min(e),
                text: c.text,
            })
            .filter(|c| c.end > c.start)
            .collect()
    }

    /// Forgets finished cues that end by `t`.
    pub fn prune(&mut self, t: u64) {
        self.closed.retain(|c| c.end > t);
    }
}

/// `HH:MM:SS.mmm` for a 90 kHz duration.
fn vtt_time(t90: u64) -> String {
    let ms = t90 / 90;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One WebVTT segment starting at unwrapped TS time `start`; LOCAL 0 maps
/// to `start` (sent as a 33-bit PTS).
pub(crate) fn vtt_segment(start: u64, cues: &[Cue]) -> String {
    let mut out = format!(
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{},LOCAL:00:00:00.000\n",
        start % WRAP
    );
    for c in cues {
        let _ = write!(
            out,
            "\n{} --> {}\n{}\n",
            vtt_time(c.start.saturating_sub(start)),
            vtt_time(c.end.saturating_sub(start)),
            escape(&c.text)
        );
    }
    out
}

/// The master playlist: one video variant, one subtitle rendition per language.
pub(crate) fn master_playlist(langs: &[VttLang], bandwidth: u64) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
    for l in langs {
        let _ = writeln!(
            out,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"{c}\",NAME=\"{n}\",DEFAULT={d},AUTOSELECT=YES,URI=\"sub_{c}.m3u8\"",
            c = l.code,
            n = endonym(&l.code).replace('"', "'"),
            d = if l.source { "YES" } else { "NO" },
        );
    }
    let subs = if langs.is_empty() {
        ""
    } else {
        ",SUBTITLES=\"subs\""
    };
    let _ = write!(
        out,
        "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}{subs},CLOSED-CAPTIONS=NONE\nvideo.m3u8\n"
    );
    out
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seg {
    pub seq: u64,
    pub dur: f64,
    pub uri: String,
}

/// Target duration and segments of a media playlist.
pub(crate) fn parse_playlist(text: &str) -> (u64, Vec<Seg>) {
    let (mut target, mut seq, mut dur) = (0u64, 0u64, None);
    let mut segs = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = v.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().and_then(|d| d.parse::<f64>().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(d) = dur.take() {
                segs.push(Seg {
                    seq,
                    dur: d,
                    uri: line.to_string(),
                });
            }
            seq += 1;
        }
    }
    (target, segs)
}

fn sub_playlist(code: &str, target: u64, segs: &[Seg]) -> String {
    let first = segs.first().map_or(0, |s| s.seq);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{first}\n"
    );
    for s in segs {
        let _ = write!(out, "#EXTINF:{:.3},\nsub_{code}_{:05}.vtt\n", s.dur, s.seq);
    }
    out
}

/// PTS of the first video PES in MPEG-TS `data`.
pub(crate) fn first_video_pts(data: &[u8]) -> Option<u64> {
    for p in data.chunks_exact(188) {
        if p.first() != Some(&0x47) || p.get(1).is_none_or(|b| b & 0x40 == 0) {
            continue;
        }
        let afc = (p.get(3)? >> 4) & 3;
        let off = if afc & 2 != 0 {
            5 + usize::from(*p.get(4)?)
        } else {
            4
        };
        let Some(pes) = p.get(off..).filter(|_| afc & 1 != 0) else {
            continue;
        };
        if pes.len() < 14 || pes[..3] != [0, 0, 1] || !(0xE0..=0xEF).contains(&pes[3]) {
            continue;
        }
        if pes[7] & 0x80 == 0 {
            continue;
        }
        let b: Vec<u64> = pes[9..14].iter().map(|x| u64::from(*x)).collect();
        return Some(
            ((b[0] >> 1) & 7) << 30 | b[1] << 22 | (b[2] >> 1) << 15 | b[3] << 7 | b[4] >> 1,
        );
    }
    None
}

/// The unwrapped time closest to `near` whose low 33 bits are `p`.
fn unwrap33(p: u64, near: u64) -> u64 {
    let base = near - near % WRAP + p % WRAP;
    [base.checked_sub(WRAP), Some(base), base.checked_add(WRAP)]
        .into_iter()
        .flatten()
        .min_by_key(|c| c.abs_diff(near))
        .unwrap_or(base)
}

/// 90 kHz TS time of a caption shown at `due_ns`, from the last video
/// buffer (`pts_ns` seen at `mono`).
fn to_ts(anchor: (u64, u64), due_ns: u64) -> u64 {
    let (pts_ns, mono) = anchor;
    let rt = i128::from(pts_ns) + i128::from(due_ns) - i128::from(mono);
    let rt90 = u64::try_from(rt.max(0) * 9 / 100_000).unwrap_or(0);
    rt90 + TS_BASE
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Writes the WebVTT renditions and playlists of one HLS output. Owns its
/// directory: cleaned when created and removed when dropped.
pub(crate) struct HlsWriter {
    dir: PathBuf,
    params: HlsParams,
    ctx: Arc<HlsCtx>,
    cursor: u64,
    tracks: Vec<CueTrack>,
    /// A/V segments already given VTT segments, oldest first.
    done: VecDeque<Seg>,
    prev_end: Option<u64>,
    master: bool,
    io_warned: bool,
}

impl HlsWriter {
    pub fn new(params: &HlsParams, ctx: Arc<HlsCtx>) -> Result<Self> {
        let dir = output_dir(&params.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("cannot clean {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Self {
            dir,
            params: params.clone(),
            cursor: ctx.hub.cursor(),
            tracks: ctx.langs.iter().map(|_| CueTrack::default()).collect(),
            ctx,
            done: VecDeque::new(),
            prev_end: None,
            master: false,
            io_warned: false,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn io<T>(&mut self, what: &str, r: std::io::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                if !self.io_warned {
                    self.io_warned = true;
                    warn!(dir = %self.dir.display(), err = %e, "HLS: cannot write {what}");
                }
                None
            }
        }
    }

    fn probe_pts(&self, uri: &str) -> Option<u64> {
        let name = Path::new(uri).file_name()?;
        let f = std::fs::File::open(self.dir.join(name)).ok()?;
        let mut data = Vec::new();
        f.take(PTS_PROBE_BYTES).read_to_end(&mut data).ok()?;
        first_video_pts(&data)
    }

    /// Takes new caption lines, and writes VTT segments for A/V segments
    /// `hlssink2` finished since the last call. `anchor`: the last video
    /// buffer's PTS (ns) and when it was seen ([`mono_ns`]).
    pub fn tick(&mut self, anchor: Option<(u64, u64)>) {
        let (lines, next) = self.ctx.hub.since(self.cursor);
        self.cursor = next;
        if let Some(a) = anchor {
            for l in lines {
                if let Some(i) = self.ctx.langs.iter().position(|v| v.code == l.lang)
                    && let Some(t) = self.tracks.get_mut(i)
                {
                    t.line(to_ts(a, l.due_ns), &l.text, l.new_row, &self.ctx);
                }
            }
        }
        let Ok(text) = std::fs::read_to_string(self.dir.join("video.m3u8")) else {
            return;
        };
        let (target, segs) = parse_playlist(&text);
        let last = self.done.back().map(|s| s.seq);
        let new: Vec<Seg> = segs
            .iter()
            .filter(|s| last.is_none_or(|l| s.seq > l))
            .cloned()
            .collect();
        if new.is_empty() {
            return;
        }
        let hold = u64::from(self.ctx.clear_after_ms) * 90;
        let now_ts = anchor.map(|a| to_ts(a, mono_ns()));
        for seg in &new {
            let dur = (seg.dur.max(0.0) * 90_000.0) as u64;
            let near = self.prev_end.or(now_ts).unwrap_or(TS_BASE);
            let start = match self.probe_pts(&seg.uri) {
                Some(p) => unwrap33(p, near),
                None => self.prev_end.unwrap_or_else(|| near.saturating_sub(dur)),
            };
            let end = start + dur;
            for i in 0..self.tracks.len() {
                let Some(code) = self.ctx.langs.get(i).map(|l| l.code.clone()) else {
                    continue;
                };
                let body = match self.tracks.get_mut(i) {
                    Some(t) => {
                        t.expire(end, hold);
                        let cues = t.cues_in(start, end, hold);
                        t.prune(end);
                        vtt_segment(start, &cues)
                    }
                    None => continue,
                };
                let path = self.dir.join(format!("sub_{code}_{:05}.vtt", seg.seq));
                let r = write_atomic(&path, &body);
                self.io("a WebVTT segment", r);
            }
            if !self.master {
                let bytes = Path::new(&seg.uri)
                    .file_name()
                    .and_then(|n| std::fs::metadata(self.dir.join(n)).ok())
                    .map_or(0, |m| m.len());
                let bw = (bytes * 8 * 12 / 10) as f64 / seg.dur.max(0.5);
                let text = master_playlist(&self.ctx.langs, (bw as u64).max(100_000));
                let r = write_atomic(&self.dir.join("master.m3u8"), &text);
                if self.io("the master playlist", r).is_some() {
                    self.master = true;
                    info!(name = %self.params.name, langs = self.ctx.langs.len(), "HLS output live");
                }
            }
            self.prev_end = Some(end);
            self.done.push_back(seg.clone());
        }
        // The subtitle playlists list exactly the A/V window.
        let first = segs.first().map_or(0, |s| s.seq);
        let window: Vec<Seg> = self
            .done
            .iter()
            .filter(|s| s.seq >= first)
            .cloned()
            .collect();
        let target = window
            .iter()
            .map(|s| s.dur.ceil() as u64)
            .fold(target.max(u64::from(self.params.segment_s)), u64::max);
        for i in 0..self.ctx.langs.len() {
            let Some(code) = self.ctx.langs.get(i).map(|l| l.code.clone()) else {
                continue;
            };
            let r = write_atomic(
                &self.dir.join(format!("sub_{code}.m3u8")),
                &sub_playlist(&code, target, &window),
            );
            self.io("a subtitle playlist", r);
        }
        // Keep two segments past the window for players still fetching them.
        while self.done.front().is_some_and(|s| s.seq + 2 < first) {
            if let Some(old) = self.done.pop_front() {
                for l in &self.ctx.langs {
                    let _ = std::fs::remove_file(
                        self.dir.join(format!("sub_{}_{:05}.vtt", l.code, old.seq)),
                    );
                }
            }
        }
    }
}

impl Drop for HlsWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> HlsCtx {
        HlsCtx {
            hub: Arc::new(TextHub::default()),
            langs: vec![
                VttLang {
                    code: "en".into(),
                    source: true,
                },
                VttLang {
                    code: "ar".into(),
                    source: false,
                },
            ],
            clear_after_ms: 4000,
            rows: 2,
            max_chars: 32,
        }
    }

    const S: u64 = 90_000;

    #[test]
    fn cues_cross_segment_boundaries_and_clear() {
        let c = ctx();
        let hold = 4 * S;
        let mut t = CueTrack::default();
        let base = TS_BASE + 10 * S;
        t.line(base + S / 2, "hello", true, &c);
        t.line(base + 3 * S / 2, "world", false, &c);
        // Segment [base, base+2 s): first cue whole, second clipped.
        let seg1 = t.cues_in(base, base + 2 * S, hold);
        assert_eq!(seg1.len(), 2);
        assert_eq!((seg1[0].start, seg1[0].end), (base + S / 2, base + 3 * S / 2));
        assert_eq!(seg1[1].text, "hello world");
        assert_eq!(seg1[1].end, base + 2 * S);
        t.prune(base + 2 * S);
        // The next segment gets the rest of the showing line.
        let seg2 = t.cues_in(base + 2 * S, base + 4 * S, hold);
        assert_eq!(seg2.len(), 1);
        assert_eq!(seg2[0].start, base + 2 * S);
        // Cleared after 4 s: ends at 5.5 s, and the next line starts fresh.
        t.expire(base + 6 * S, hold);
        let seg3 = t.cues_in(base + 4 * S, base + 6 * S, hold);
        assert_eq!(seg3[0].end, base + 3 * S / 2 + hold);
        t.line(base + 7 * S, "again", false, &c);
        assert_eq!(t.cues_in(base + 6 * S, base + 8 * S, hold)[0].text, "again");
    }

    #[test]
    fn rows_roll_and_cjk_joins_without_space() {
        let c = ctx();
        let mut t = CueTrack::default();
        t.line(TS_BASE, "one", true, &c);
        t.line(TS_BASE + 1, "two", true, &c);
        t.line(TS_BASE + 2, "three", true, &c);
        let cues = t.cues_in(TS_BASE, TS_BASE + S, 4 * S);
        assert_eq!(cues.last().map(|c| c.text.as_str()), Some("two\nthree"));
        let mut t = CueTrack::default();
        t.line(TS_BASE, "こんにちは", true, &c);
        t.line(TS_BASE + 1, "世界", false, &c);
        let cues = t.cues_in(TS_BASE, TS_BASE + S, 4 * S);
        assert_eq!(cues.last().map(|c| c.text.as_str()), Some("こんにちは世界"));
    }

    #[test]
    fn vtt_segment_text() {
        let start = TS_BASE + 12 * S;
        let cues = vec![
            Cue {
                start: start + S / 4,
                end: start + 3 * S / 2,
                text: "a <b> & c".into(),
            },
            Cue {
                start: start + 3 * S / 2,
                end: start + 2 * S,
                text: "مرحبا بالعالم\nこんにちは".into(),
            },
        ];
        let v = vtt_segment(start, &cues);
        assert_eq!(
            v,
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:325080000,LOCAL:00:00:00.000\n\n\
             00:00:00.250 --> 00:00:01.500\na &lt;b&gt; &amp; c\n\n\
             00:00:01.500 --> 00:00:02.000\nمرحبا بالعالم\nこんにちは\n"
        );
        // MPEGTS is a 33-bit PTS.
        assert!(vtt_segment(WRAP + 5, &[]).contains("MPEGTS:5,"));
    }

    #[test]
    fn master_playlist_text() {
        let m = master_playlist(&ctx().langs, 2_500_000);
        assert_eq!(
            m,
            "#EXTM3U\n#EXT-X-VERSION:3\n\
             #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",NAME=\"English\",DEFAULT=YES,AUTOSELECT=YES,URI=\"sub_en.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"ar\",NAME=\"العربية\",DEFAULT=NO,AUTOSELECT=YES,URI=\"sub_ar.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2500000,SUBTITLES=\"subs\",CLOSED-CAPTIONS=NONE\nvideo.m3u8\n"
        );
    }

    #[test]
    fn playlist_parse_and_sub_playlist() {
        let (t, s) = parse_playlist(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-TARGETDURATION:2\n\n#EXTINF:2.0333,\nv00007.ts\n#EXTINF:1.9,\nv00008.ts\n",
        );
        assert_eq!(t, 2);
        assert_eq!(s.len(), 2);
        assert_eq!((s[1].seq, s[1].uri.as_str()), (8, "v00008.ts"));
        let p = sub_playlist("es", 3, &s);
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:2.033,\nsub_es_00007.vtt\n"));
    }

    #[test]
    fn pts_helpers() {
        // One TS packet: PUSI, payload only, video PES with PTS 0x1_2345_6789.
        let pts: u64 = 0x1_2345_6789;
        let mut p = vec![0x47, 0x41, 0x00, 0x10, 0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5];
        p.extend([
            0x21 | (((pts >> 30) & 7) << 1) as u8,
            (pts >> 22) as u8,
            (((pts >> 15) & 0x7f) << 1) as u8 | 1,
            (pts >> 7) as u8,
            ((pts & 0x7f) << 1) as u8 | 1,
        ]);
        p.resize(188, 0xff);
        assert_eq!(first_video_pts(&p), Some(pts));
        assert_eq!(unwrap33(5, WRAP - 10), WRAP + 5);
        assert_eq!(unwrap33(WRAP - 5, WRAP + 3), WRAP - 5);
        // 1 s after the anchor frame at PTS 10 s.
        assert_eq!(
            to_ts((10_000_000_000, 500), 1_000_000_500),
            TS_BASE + 11 * S
        );
    }
}
