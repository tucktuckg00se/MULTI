//! `latency report`: matches frames between two tap CSVs and summarises delay.
//!
//! Matching (default `--match auto`):
//! 1. By content: a frame's `tail_hash` (hash of the last 128 payload bytes of its
//!    PES) survives pass-through even if the system rewrites PTS or inserts SEI
//!    NAL units ahead of the slices. For duplicate hashes the input frame with the
//!    latest wallclock not after the output frame is used.
//! 2. The PTS offset is the most common `(out_pts - in_pts) mod 2^33` over the
//!    content-matched pairs. Pairs that disagree with it are counted.
//! 3. If fewer than half the output frames match by content (the system changed
//!    the tail bytes, e.g. re-encoding), frames are matched by PTS instead, using
//!    `--pts-offset` if given, else the offset from step 2 if there is one, else 0.

use crate::stats::{Summary, slope};
use crate::ts::PTS_WRAP;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub wall: i64,
    pub pts: Option<i64>,
    pub seq: u64,
    pub hash: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum MatchMode {
    Auto,
    Hash,
    Pts,
}

pub struct ReportArgs<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub mode: MatchMode,
    pub pts_offset: Option<i64>,
    pub json: bool,
    pub pairs: Option<&'a Path>,
    pub skip_seconds: f64,
}

pub fn load(path: &Path) -> Result<(Vec<Row>, usize)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut rows = Vec::new();
    let mut bad = 0;
    for (i, line) in text.lines().enumerate() {
        if i == 0 && line.starts_with("wallclock_ns") {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 3 {
            bad += 1;
            continue;
        }
        let (Ok(wall), Ok(seq)) = (f[0].parse::<i64>(), f[2].parse::<u64>()) else {
            bad += 1;
            continue;
        };
        let pts = f[1].parse::<i64>().ok();
        let hash = f.get(3).and_then(|h| u64::from_str_radix(h, 16).ok()).unwrap_or(0);
        rows.push(Row { wall, pts, seq, hash });
    }
    Ok((rows, bad))
}

/// Signed difference a - b modulo 2^33, in [-2^32, 2^32).
pub fn pts_diff(a: i64, b: i64) -> i64 {
    (a - b + PTS_WRAP / 2).rem_euclid(PTS_WRAP) - PTS_WRAP / 2
}

pub struct Matched {
    pub pairs: Vec<(usize, usize)>,
    pub method: &'static str,
    pub pts_offset: Option<i64>,
    pub offset_agree: usize,
    pub offset_checked: usize,
}

fn pick(cands: &[usize], inp: &[Row], out_wall: i64) -> Option<usize> {
    // Latest input not after the output; else the earliest one.
    cands
        .iter()
        .copied()
        .filter(|&i| inp[i].wall <= out_wall)
        .max_by_key(|&i| inp[i].wall)
        .or_else(|| cands.iter().copied().min_by_key(|&i| inp[i].wall))
}

fn match_hash(inp: &[Row], out: &[Row]) -> Vec<(usize, usize)> {
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, r) in inp.iter().enumerate() {
        if r.hash != 0 {
            by_hash.entry(r.hash).or_default().push(i);
        }
    }
    let mut pairs = Vec::new();
    for (j, r) in out.iter().enumerate() {
        if r.hash == 0 {
            continue;
        }
        if let Some(c) = by_hash.get(&r.hash)
            && let Some(i) = pick(c, inp, r.wall)
        {
            pairs.push((i, j));
        }
    }
    pairs
}

fn match_pts(inp: &[Row], out: &[Row], offset: i64) -> Vec<(usize, usize)> {
    let mut by_pts: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in inp.iter().enumerate() {
        if let Some(p) = r.pts {
            by_pts.entry(p.rem_euclid(PTS_WRAP)).or_default().push(i);
        }
    }
    let mut pairs = Vec::new();
    for (j, r) in out.iter().enumerate() {
        let Some(p) = r.pts else { continue };
        let key = (p - offset).rem_euclid(PTS_WRAP);
        if let Some(c) = by_pts.get(&key)
            && let Some(i) = pick(c, inp, r.wall)
        {
            pairs.push((i, j));
        }
    }
    pairs
}

pub fn match_frames(inp: &[Row], out: &[Row], mode: MatchMode, forced_offset: Option<i64>) -> Matched {
    let hp = if mode == MatchMode::Pts { Vec::new() } else { match_hash(inp, out) };
    // Offset = mode of PTS differences over content matches.
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for &(i, j) in &hp {
        if let (Some(a), Some(b)) = (out[j].pts, inp[i].pts) {
            *counts.entry(pts_diff(a, b)).or_default() += 1;
        }
    }
    let checked: usize = counts.values().sum();
    let detected = counts.iter().max_by_key(|(k, v)| (**v, -(k.abs()))).map(|(k, v)| (*k, *v));
    let use_hash = match mode {
        MatchMode::Hash => true,
        MatchMode::Pts => false,
        MatchMode::Auto => hp.len() * 2 >= out.len() && !hp.is_empty(),
    };
    if use_hash {
        return Matched {
            pairs: hp,
            method: "content hash",
            pts_offset: forced_offset.or(detected.map(|d| d.0)),
            offset_agree: detected.map_or(0, |d| d.1),
            offset_checked: checked,
        };
    }
    let off = forced_offset.or(detected.map(|d| d.0)).unwrap_or(0);
    let pairs = match_pts(inp, out, off);
    Matched {
        offset_agree: pairs.len(),
        offset_checked: pairs.len(),
        pairs,
        method: "pts",
        pts_offset: Some(off),
    }
}

pub fn run(a: ReportArgs) -> Result<()> {
    let (inp, bad_in) = load(a.input)?;
    let (out, bad_out) = load(a.output)?;
    if inp.is_empty() || out.is_empty() {
        bail!("no frames: input {} rows, output {} rows", inp.len(), out.len());
    }
    let m = match_frames(&inp, &out, a.mode, a.pts_offset);
    // Warm-up skip: drop pairs whose input arrived in the first N seconds.
    let t0 = inp[0].wall;
    let skip = (a.skip_seconds * 1e9) as i64;
    let pairs: Vec<(usize, usize)> = m.pairs.iter().copied().filter(|&(i, _)| inp[i].wall - t0 >= skip).collect();
    if pairs.is_empty() {
        bail!("no frames matched (method {})", m.method);
    }
    let delays_ms: Vec<f64> = pairs.iter().map(|&(i, j)| (out[j].wall - inp[i].wall) as f64 / 1e6).collect();
    let times_s: Vec<f64> = pairs.iter().map(|&(i, _)| (inp[i].wall - t0) as f64 / 1e9).collect();
    let s = Summary::of(&delays_ms);
    // Jitter: std-dev of delay, and mean |delay[k] - delay[k-1]| (RFC 3550 style, unsmoothed).
    let succ: Vec<f64> = delays_ms.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let succ_mean = if succ.is_empty() { 0.0 } else { succ.iter().sum::<f64>() / succ.len() as f64 };
    let drift_ms_per_min = slope(&times_s, &delays_ms) * 60.0;

    // Unmatched counts inside the window both taps saw.
    let in_lo = pairs.iter().map(|p| inp[p.0].wall).min().unwrap_or(0);
    let in_hi = pairs.iter().map(|p| inp[p.0].wall).max().unwrap_or(0);
    let out_lo = pairs.iter().map(|p| out[p.1].wall).min().unwrap_or(0);
    let out_hi = pairs.iter().map(|p| out[p.1].wall).max().unwrap_or(0);
    let mut in_used = vec![false; inp.len()];
    let mut out_used = vec![false; out.len()];
    for &(i, j) in &pairs {
        in_used[i] = true;
        out_used[j] = true;
    }
    let unmatched_in = (0..inp.len()).filter(|&i| !in_used[i] && inp[i].wall >= in_lo && inp[i].wall <= in_hi).count();
    let unmatched_out = (0..out.len()).filter(|&j| !out_used[j] && out[j].wall >= out_lo && out[j].wall <= out_hi).count();
    let dup_in = pairs.len() - {
        let mut v: Vec<usize> = pairs.iter().map(|p| p.0).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };

    if let Some(p) = a.pairs {
        let mut t = String::from("in_seq,out_seq,in_pts,out_pts,in_wall_ns,delay_ms\n");
        for (k, &(i, j)) in pairs.iter().enumerate() {
            let _ = writeln!(
                t,
                "{},{},{},{},{},{:.3}",
                inp[i].seq,
                out[j].seq,
                inp[i].pts.map_or(String::new(), |v| v.to_string()),
                out[j].pts.map_or(String::new(), |v| v.to_string()),
                inp[i].wall,
                delays_ms[k]
            );
        }
        std::fs::write(p, t).with_context(|| format!("write {}", p.display()))?;
    }

    let off = m.pts_offset.unwrap_or(0);
    if a.json {
        println!(
            "{{\"method\":\"{}\",\"pts_offset_90k\":{},\"pts_offset_ms\":{:.3},\"offset_agree\":{},\"offset_checked\":{},\
\"frames_in\":{},\"frames_out\":{},\"matched\":{},\"unmatched_in\":{},\"unmatched_out\":{},\"duplicate_in_matches\":{},\
\"bad_rows\":{},\"delay_ms\":{{\"min\":{:.3},\"p50\":{:.3},\"p95\":{:.3},\"p99\":{:.3},\"max\":{:.3},\"mean\":{:.3}}},\
\"jitter_ms\":{{\"stddev\":{:.3},\"mean_abs_successive\":{:.3}}},\"drift_ms_per_min\":{:.3}}}",
            m.method, off, off as f64 / 90.0, m.offset_agree, m.offset_checked, inp.len(), out.len(), pairs.len(),
            unmatched_in, unmatched_out, dup_in, bad_in + bad_out, s.min, s.p50, s.p95, s.p99, s.max, s.mean,
            s.stddev, succ_mean, drift_ms_per_min
        );
    } else {
        println!("frames: in={} out={} matched={} (method: {})", inp.len(), out.len(), pairs.len(), m.method);
        println!("unmatched in window: in={unmatched_in} out={unmatched_out}  duplicate input matches={dup_in}  bad CSV rows={}", bad_in + bad_out);
        println!(
            "pts offset: {} ticks ({:.3} ms){}",
            off,
            off as f64 / 90.0,
            if m.offset_checked > 0 {
                format!(", {}/{} content-matched frames agree", m.offset_agree, m.offset_checked)
            } else {
                String::new()
            }
        );
        println!(
            "delay ms: p50={:.3} p95={:.3} p99={:.3} max={:.3} mean={:.3} min={:.3}",
            s.p50, s.p95, s.p99, s.max, s.mean, s.min
        );
        println!(
            "jitter ms: stddev={:.3} mean|successive diff|={:.3}  drift={:.3} ms/min",
            s.stddev, succ_mean, drift_ms_per_min
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pts0: i64, wall0: i64, n: usize, hash_base: u64) -> Vec<Row> {
        (0..n)
            .map(|k| Row {
                wall: wall0 + k as i64 * 33_333_333,
                pts: Some((pts0 + k as i64 * 3000).rem_euclid(PTS_WRAP)),
                seq: k as u64 + 1,
                hash: hash_base + k as u64,
            })
            .collect()
    }

    #[test]
    fn detects_offset_across_wrap() {
        let inp = rows(PTS_WRAP - 30_000, 0, 100, 1000);
        // Output shifted +90000 ticks (wraps), 50 ms later, first 3 frames lost.
        let out: Vec<Row> = rows(PTS_WRAP - 30_000 + 90_000, 50_000_000, 100, 1000).into_iter().skip(3).collect();
        let m = match_frames(&inp, &out, MatchMode::Auto, None);
        assert_eq!(m.method, "content hash");
        assert_eq!(m.pts_offset, Some(90_000));
        assert_eq!(m.pairs.len(), 97);
        for &(i, j) in &m.pairs {
            assert_eq!(out[j].wall - inp[i].wall, 50_000_000);
        }
        // PTS-only matching with the detected offset gives the same pairs.
        let p = match_frames(&inp, &out, MatchMode::Pts, Some(90_000));
        assert_eq!(p.pairs, m.pairs);
    }

    #[test]
    fn falls_back_to_pts_when_hashes_differ() {
        let inp = rows(0, 0, 50, 1);
        let out = rows(0, 10_000_000, 50, 9999);
        let m = match_frames(&inp, &out, MatchMode::Auto, None);
        assert_eq!(m.method, "pts");
        assert_eq!(m.pairs.len(), 50);
    }

    #[test]
    fn diff_mod() {
        assert_eq!(pts_diff(5, PTS_WRAP - 5), 10);
        assert_eq!(pts_diff(PTS_WRAP - 5, 5), -10);
    }
}
