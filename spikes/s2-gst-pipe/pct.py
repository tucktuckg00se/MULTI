#!/usr/bin/env python3
"""Percentiles of s2-gst-pipe's per-frame delay CSV: pct.py delay.csv [skip_s]."""
import csv, sys

rows = list(csv.DictReader(open(sys.argv[1])))
skip = float(sys.argv[2]) if len(sys.argv) > 2 else 0
if not rows:
    print("no rows"); sys.exit()
t0 = int(rows[0]["mux_in_ns"])
rows = [r for r in rows if int(r["mux_in_ns"]) - t0 >= skip * 1e9]

def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, int(round(p / 100 * len(v) + 0.5)) - 1))] if v else float("nan")

for name, key in (("entry->mux ms", "delay_us"), ("caption stage ms", "cc_stage_us")):
    v = [int(r[key]) / 1000 for r in rows if int(r[key]) >= 0]
    if v:
        print(f"{name}: n={len(v)} p50={pct(v,50):.3f} p95={pct(v,95):.3f} p99={pct(v,99):.3f} max={max(v):.3f}")
skew = [(int(r["out_pts_ns"]) - int(r["bridge_rt_ns"])) / 1e6 for r in rows]
if len(skew) > 1:
    dur_min = (int(rows[-1]["mux_in_ns"]) - int(rows[0]["mux_in_ns"])) / 60e9
    print(f"out_pts - running_time ms: first={skew[0]:.1f} last={skew[-1]:.1f} drift={(skew[-1]-skew[0])/max(dur_min,1e-9):.2f} ms/min sessions={len(set(r['session'] for r in rows))}")
