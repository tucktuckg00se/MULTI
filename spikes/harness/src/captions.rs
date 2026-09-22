//! `latency captions`: caption lag against a reference transcript.
//!
//! Timeline: SRT cue times are converted to stream time with
//! `t = cue_time - pts_origin`. `source.sh` starts audio and video together at
//! stream time 0 and loops the audio, so audio time = t mod loop_seconds, and
//! utterance `u` in loop `k` ends at stream time `end_u + k * loop_seconds`.
//!
//! For each utterance occurrence the target is its last N content words
//! (N = 3; stop-words removed; lowercase, punctuation stripped). The first cue
//! (by start time) in `[ref_end - pre, ref_end + max_lag]` whose content words
//! contain the target's last word plus enough of the others, in order, within a
//! short span, and with a small per-word edit distance, is the appearance.
//! lag = cue start - reference end.

use crate::stats::Summary;
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub struct CaptionArgs {
    pub srt: PathBuf,
    pub segments: PathBuf,
    pub words: Option<PathBuf>,
    pub loop_seconds: Option<f64>,
    pub wav: Option<PathBuf>,
    pub pts_origin: f64,
    pub n_words: usize,
    pub max_lag: f64,
    pub pre: f64,
    pub csv: Option<PathBuf>,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Word {
    pub start: f64,
    pub end: f64,
}

fn parse_ts(s: &str) -> Option<f64> {
    // HH:MM:SS,mmm (also accepts '.')
    let s = s.trim();
    let (hms, ms) = s.split_once([',', '.'])?;
    let mut it = hms.split(':');
    let h: f64 = it.next()?.trim().parse().ok()?;
    let m: f64 = it.next()?.parse().ok()?;
    let sec: f64 = it.next()?.parse().ok()?;
    let frac: f64 = format!("0.{ms}").parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec + frac)
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth_angle = false;
    let mut depth_brace = false;
    for c in s.chars() {
        match c {
            '<' => depth_angle = true,
            '>' if depth_angle => depth_angle = false,
            '{' => depth_brace = true,
            '}' if depth_brace => depth_brace = false,
            _ if depth_angle || depth_brace => {}
            _ => out.push(c),
        }
    }
    out
}

pub fn parse_srt(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let text = text.replace("\r\n", "\n");
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| l.trim().is_empty());
        let mut time_line = None;
        for l in lines.by_ref() {
            if l.contains("-->") {
                time_line = Some(l);
                break;
            }
        }
        let Some(tl) = time_line else { continue };
        let Some((a, b)) = tl.split_once("-->") else { continue };
        let b = b.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_ts(a), parse_ts(b)) else { continue };
        let body: Vec<String> = lines.map(strip_tags).collect();
        cues.push(Cue {
            start,
            end,
            text: body.join(" "),
        });
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    cues
}

pub fn parse_segments(text: &str) -> Vec<Segment> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let start = f.next()?.trim().parse().ok()?;
            let end = f.next()?.trim().parse().ok()?;
            let text = f.next().unwrap_or("").to_string();
            Some(Segment { start, end, text })
        })
        .collect()
}

/// `word<TAB>start_s<TAB>end_s` (header line optional).
pub fn parse_words(text: &str) -> Vec<Word> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 3 {
                return None;
            }
            Some(Word {
                start: f[1].trim().parse().ok()?,
                end: f[2].trim().parse().ok()?,
            })
        })
        .collect()
}

/// Duration of a PCM WAV file from its header (data bytes / byte rate).
pub fn wav_duration(path: &Path) -> Result<f64> {
    let d = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if d.len() < 12 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        bail!("{} is not a RIFF/WAVE file", path.display());
    }
    let mut i = 12;
    let mut byte_rate = 0u32;
    while i + 8 <= d.len() {
        let id = &d[i..i + 4];
        let size = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
        if id == b"fmt " && i + 20 <= d.len() {
            byte_rate = u32::from_le_bytes([d[i + 16], d[i + 17], d[i + 18], d[i + 19]]);
        } else if id == b"data" {
            if byte_rate == 0 {
                bail!("WAV data chunk before fmt chunk");
            }
            let size = size.min(d.len() - i - 8);
            return Ok(size as f64 / byte_rate as f64);
        }
        i += 8 + size + (size & 1);
    }
    bail!("no data chunk in {}", path.display())
}

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "at", "by", "for", "with", "from", "as",
    "is", "was", "are", "were", "be", "been", "am", "it", "its", "this", "that", "these", "those", "he", "she",
    "they", "we", "you", "i", "me", "him", "her", "them", "us", "my", "his", "their", "our", "your", "not", "no",
    "so", "do", "did", "had", "has", "have", "then", "there", "which", "who", "what", "all", "one", "into",
    "upon", "up", "out", "would", "could", "should", "will", "shall", "can", "may", "might", "must", "s",
];

pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

pub fn content_words(s: &str) -> Vec<String> {
    tokens(s).into_iter().filter(|w| !STOP.contains(&w.as_str())).collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Words match if the edit distance is within 0 (<=2 chars), 1 (3-7) or 2 (8+).
pub fn word_eq(a: &str, b: &str) -> bool {
    let len = a.chars().count().max(b.chars().count());
    let allow = match len {
        0..=2 => 0,
        3..=7 => 1,
        _ => 2,
    };
    a == b || levenshtein(a, b) <= allow
}

/// Returns the number of target words found (0 if the last target word is not
/// found). Target words must appear in order, the others within `len + 1`
/// content words before the last one.
pub fn match_count(target: &[String], cue: &[String]) -> usize {
    let Some(last) = target.last() else { return 0 };
    let mut best = 0;
    for e in (0..cue.len()).filter(|&e| word_eq(&cue[e], last)) {
        let lo = e.saturating_sub(target.len() + 1);
        let mut n = 1;
        let mut pos = e;
        for t in target[..target.len() - 1].iter().rev() {
            if let Some(p) = (lo..pos).rev().find(|&p| word_eq(&cue[p], t)) {
                n += 1;
                pos = p;
            }
        }
        best = best.max(n);
    }
    best
}

pub fn required(n: usize) -> usize {
    if n <= 2 { n } else { n - 1 }
}

pub struct Occurrence {
    pub loop_k: i64,
    pub utt: usize,
    pub start: f64,
    pub end: f64,
    pub ref_end: f64,
    pub target: Vec<String>,
    pub appear: Option<f64>,
    pub matched_words: usize,
}

pub fn run(a: CaptionArgs) -> Result<()> {
    let srt_text = std::fs::read_to_string(&a.srt).with_context(|| format!("read {}", a.srt.display()))?;
    let seg_text = std::fs::read_to_string(&a.segments).with_context(|| format!("read {}", a.segments.display()))?;
    let mut cues = parse_srt(&srt_text);
    let segs = parse_segments(&seg_text);
    if segs.is_empty() {
        bail!("no segments in {}", a.segments.display());
    }
    if cues.is_empty() {
        bail!("no cues in {}", a.srt.display());
    }
    for c in &mut cues {
        c.start -= a.pts_origin;
        c.end -= a.pts_origin;
    }
    let words = match &a.words {
        Some(p) => Some(parse_words(&std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?)),
        None => None,
    };
    let loop_len = match (a.loop_seconds, &a.wav) {
        (Some(l), _) => l,
        (None, Some(w)) => wav_duration(w)?,
        (None, None) => segs.iter().map(|s| s.end).fold(0.0, f64::max),
    };
    if loop_len <= 0.0 {
        bail!("loop length must be > 0");
    }
    let cue_words: Vec<Vec<String>> = cues.iter().map(|c| content_words(&c.text)).collect();
    let cov_start = cues[0].start;
    let cov_end = cues.iter().map(|c| c.end).fold(f64::MIN, f64::max);

    let mut occ = Vec::new();
    let k_lo = (cov_start / loop_len).floor() as i64 - 1;
    let k_hi = (cov_end / loop_len).ceil() as i64;
    for k in k_lo..=k_hi {
        let shift = k as f64 * loop_len;
        for (u, s) in segs.iter().enumerate() {
            let start = s.start + shift;
            let end = s.end + shift;
            // Only utterances fully inside the captioned span (with room for lag).
            if start < cov_start || end + a.max_lag > cov_end {
                continue;
            }
            let ref_end = words
                .as_ref()
                .and_then(|w| {
                    w.iter()
                        .filter(|w| w.start >= s.start && w.start < s.end)
                        .map(|w| w.end)
                        .reduce(f64::max)
                })
                .map_or(end, |e| e + shift);
            // Last N content words of the running transcript: short utterances borrow
            // words from the ones before them (wrapping at the loop point).
            let mut cw = content_words(&s.text);
            let mut back = 1;
            while cw.len() < a.n_words && back < segs.len().min(4) {
                let prev = &segs[(u + segs.len() - back) % segs.len()];
                let mut p = content_words(&prev.text);
                p.extend(cw);
                cw = p;
                back += 1;
            }
            if cw.is_empty() {
                cw = tokens(&s.text);
            }
            let target: Vec<String> = cw[cw.len().saturating_sub(a.n_words)..].to_vec();
            if target.is_empty() {
                continue;
            }
            let need = required(target.len());
            let mut appear = None;
            let mut mw = 0;
            for (ci, c) in cues.iter().enumerate() {
                if c.start < ref_end - a.pre {
                    continue;
                }
                if c.start > ref_end + a.max_lag {
                    break;
                }
                let n = match_count(&target, &cue_words[ci]);
                if n >= need {
                    appear = Some(c.start);
                    mw = n;
                    break;
                }
            }
            occ.push(Occurrence {
                loop_k: k,
                utt: u,
                start,
                end,
                ref_end,
                target,
                appear,
                matched_words: mw,
            });
        }
    }
    if occ.is_empty() {
        bail!("no utterance lies fully inside the captioned span {cov_start:.3}..{cov_end:.3} s (stream time); check --pts-origin");
    }

    let lags: Vec<f64> = occ.iter().filter_map(|o| o.appear.map(|t| t - o.ref_end)).collect();
    let s = Summary::of(&lags);
    let pct = 100.0 * lags.len() as f64 / occ.len() as f64;

    if let Some(p) = &a.csv {
        let mut t = String::from("loop,utt,utt_start_s,utt_end_s,ref_end_s,appear_s,lag_s,words_matched,target\n");
        for o in &occ {
            let _ = writeln!(
                t,
                "{},{},{:.3},{:.3},{:.3},{},{},{},{}",
                o.loop_k,
                o.utt + 1,
                o.start,
                o.end,
                o.ref_end,
                o.appear.map_or(String::new(), |v| format!("{v:.3}")),
                o.appear.map_or(String::new(), |v| format!("{:.3}", v - o.ref_end)),
                o.matched_words,
                o.target.join(" ")
            );
        }
        std::fs::write(p, t).with_context(|| format!("write {}", p.display()))?;
    }
    let refk = if words.is_some() { "last word end (--words)" } else { "segment end" };
    if a.json {
        println!(
            "{{\"utterances\":{},\"matched\":{},\"matched_pct\":{:.1},\"reference\":\"{}\",\"loop_seconds\":{:.3},\"pts_origin\":{:.6},\
\"lag_s\":{{\"min\":{:.3},\"p50\":{:.3},\"p95\":{:.3},\"max\":{:.3},\"mean\":{:.3}}}}}",
            occ.len(), lags.len(), pct, refk, loop_len, a.pts_origin, s.min, s.p50, s.p95, s.max, s.mean
        );
    } else {
        println!(
            "captions: {} cues, span {:.3}..{:.3} s stream time; loop {:.3} s; reference = {}",
            cues.len(), cov_start, cov_end, loop_len, refk
        );
        println!("utterances: {} in span, {} matched ({:.1}%)", occ.len(), lags.len(), pct);
        if s.n > 0 {
            println!(
                "lag s: p50={:.3} p95={:.3} max={:.3} mean={:.3} min={:.3}",
                s.p50, s.p95, s.max, s.mean, s.min
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_parse_and_tags() {
        let s = "1\n00:00:01,500 --> 00:00:02,000\n<i>HELLO</i> {\\an7}world\n\n2\r\n00:01:00,000 --> 00:01:01,250\r\nsecond\r\n";
        let c = parse_srt(s);
        assert_eq!(c.len(), 2);
        assert!((c[0].start - 1.5).abs() < 1e-9);
        assert_eq!(c[0].text.trim(), "HELLO world");
        assert!((c[1].end - 61.25).abs() < 1e-9);
    }

    #[test]
    fn fuzzy_match_rules() {
        let t = content_words("HE HOPED THERE WOULD BE STEW FOR DINNER TURNIPS AND CARROTS");
        assert_eq!(&t[t.len() - 3..], ["dinner", "turnips", "carrots"]);
        let target = t[t.len() - 3..].to_vec();
        // Exact, case/punctuation-insensitive.
        assert_eq!(match_count(&target, &content_words("for dinner, turnips and carrots.")), 3);
        // One ASR error on a middle word still passes the 2-of-3 rule.
        assert_eq!(match_count(&target, &content_words("for diner turnip and carrots")), 3);
        assert_eq!(match_count(&target, &content_words("for lunch turnips and carrots")), 2);
        // Last word missing -> no match (avoids early matches on partial text).
        assert_eq!(match_count(&target, &content_words("for dinner turnips and")), 0);
        assert!(word_eq("carrots", "carots"));
        assert!(word_eq("now", "nox"));
        assert!(!word_eq("it", "is"));
        assert!(!word_eq("cat", "dog"));
    }

    #[test]
    fn wav_header() {
        let mut d = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        d.extend(16u32.to_le_bytes());
        d.extend([1, 0, 1, 0]);
        d.extend(16000u32.to_le_bytes());
        d.extend(32000u32.to_le_bytes());
        d.extend([2, 0, 16, 0]);
        d.extend(b"data");
        d.extend(64000u32.to_le_bytes());
        d.extend(vec![0u8; 64000]);
        let p = std::env::temp_dir().join(format!("latency-wav-{}.wav", std::process::id()));
        std::fs::write(&p, &d).unwrap();
        let dur = wav_duration(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert!((dur - 2.0).abs() < 1e-9);
    }
}
