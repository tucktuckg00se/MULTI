#!/usr/bin/env python3
"""Collect every run under $S4/runs into one CSV (stdout).

One row per run name; columns are filled from whichever files exist:
live.json / live.lag*.json / live.wer.json (real-time run), <set>.json and
<set>.wer.json (batch WER runs), junk.* (no-speech test), soak.* (long run).
"""
import csv, json, os, sys

S4 = os.environ.get("S4", os.path.expanduser("~/.cache/multi-tools/s4"))
runs = os.path.join(S4, "runs")


def j(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def words(path):
    try:
        return len(open(path).read().split())
    except OSError:
        return None


cols = ["run", "lag_seg_p50", "lag_seg_p95", "lag_seg_max", "lag_word_p50", "lag_word_p95", "lag_word_max",
        "matched_pct", "wer_long", "wer_ls", "wer_ls_pink5", "wer_ls_music5",
        "wer_long_pink10", "wer_long_pink5", "wer_long_pink0", "wer_long_music10", "wer_long_music5",
        "wer_long_music0", "wer_mls_es", "wer_mls_fr", "wer_mls_de", "wer_scotus",
        "rtf", "pass_p50_ms", "pass_p95_ms", "max_pass_ms", "max_backlog_ms", "vram_peak_mb", "rss_peak_mb",
        "cpu_pct", "load_s", "warmup_s", "partial_rev_pct", "late_suffix", "junk_words", "soak_rss_start_mb",
        "soak_rss_end_mb", "soak_vram_mb", "soak_minutes", "soak_backlog_ms", "error"]
w = csv.DictWriter(sys.stdout, cols, extrasaction="ignore")
w.writeheader()
for name in sorted(os.listdir(runs)):
    d = os.path.join(runs, name)
    r = {"run": name}
    live = j(f"{d}/live.json")
    if live:
        st = live["stats"]
        r.update(rtf=round(live["rtf"], 3), pass_p50_ms=round(live["pass_p50_ms"]), pass_p95_ms=round(live["pass_p95_ms"]),
                 max_pass_ms=round(st["max_pass_ms"]), max_backlog_ms=round(live["max_backlog_ms"]),
                 vram_peak_mb=live["res"]["vram_peak_mb"], rss_peak_mb=round(live["res"]["rss_peak_mb"]),
                 cpu_pct=round(live["res"]["cpu_pct"]), load_s=round(live["load_s"], 2),
                 warmup_s=round(live["warmup_s"], 2), late_suffix=st.get("late_suffix", 0), error=live["error"] or "")
        if st["partial_words"]:
            r["partial_rev_pct"] = round(100 * st["partial_revisions"] / st["partial_words"], 1)
    for k, f in (("seg", "live.lag.json"), ("word", "live.lagw.json")):
        g = j(f"{d}/{f}")
        if g:
            r[f"lag_{k}_p50"], r[f"lag_{k}_p95"], r[f"lag_{k}_max"] = (g["lag_s"][q] for q in ("p50", "p95", "max"))
            if k == "seg":
                r["matched_pct"] = g["matched_pct"]
    for key, f in (("wer_long", "live"), ("wer_ls", "ls"), ("wer_ls_pink5", "ls_pink5"), ("wer_ls_music5", "ls_music5"),
                   ("wer_long_pink10", "long_pink10"), ("wer_long_pink5", "long_pink5"), ("wer_long_pink0", "long_pink0"),
                   ("wer_long_music10", "long_music10"), ("wer_long_music5", "long_music5"),
                   ("wer_long_music0", "long_music0"), ("wer_mls_es", "mls_es"), ("wer_mls_fr", "mls_fr"),
                   ("wer_mls_de", "mls_de"), ("wer_scotus", "scotus")):
        g = j(f"{d}/{f}.wer.json")
        if g:
            r[key] = g["wer"]
    if "rtf" not in r:
        b = j(f"{d}/ls.json") or j(f"{d}/long_pink5.json")
        if b:
            r.update(rtf=round(b["rtf"], 3), vram_peak_mb=b["res"]["vram_peak_mb"], cpu_pct=round(b["res"]["cpu_pct"]),
                     load_s=round(b["load_s"], 2), pass_p50_ms=round(b.get("pass_p50_ms", 0)))
    if os.path.exists(f"{d}/junk.txt"):
        r["junk_words"] = words(f"{d}/junk.txt")
    s = j(f"{d}/soak.json")
    if s:
        r.update(soak_rss_start_mb=round(s["res"]["rss_start_mb"]), soak_rss_end_mb=round(s["res"]["rss_end_mb"]),
                 soak_vram_mb=s["res"]["vram_peak_mb"], soak_minutes=round(s["audio_s"] / 60, 1),
                 soak_backlog_ms=round(s["max_backlog_ms"]))
    w.writerow(r)
