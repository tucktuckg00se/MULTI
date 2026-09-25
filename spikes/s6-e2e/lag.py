#!/usr/bin/env python3
"""Translated-caption lag from s6-e2e's --emit-log: for each clause, the time
from its last EN push to each translation's push (seconds), per lane.
Usage: lag.py emit.tsv [skip_s]"""
import sys, collections
rows = [l.rstrip('\n').split('\t') for l in open(sys.argv[1])][1:]
skip = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
t0 = int(rows[0][0]) if rows else 0
en, tr, wait = {}, collections.defaultdict(dict), collections.defaultdict(list)
for w, lane, cl, pts, wt, *_ in rows:
    t, lane, cl = (int(w) - t0) / 1e9, int(lane), int(cl)
    if t < skip: continue
    wait[lane].append(float(wt) / 1e3)
    if lane == 0: en[cl] = max(en.get(cl, 0), t)
    else: tr[lane][cl] = t
def pct(v, p):
    v = sorted(v); return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))] if v else float('nan')
print(f"clauses with EN: {len(en)}")
for lane in sorted(tr):
    d = [tr[lane][c] - en[c] for c in tr[lane] if c in en]
    miss = sum(1 for c in en if c not in tr[lane])
    print(f"lane {lane}: n={len(d)} missing={miss} delta_s p50={pct(d,50):.3f} p95={pct(d,95):.3f} max={max(d, default=float('nan')):.3f}")
for lane in sorted(wait):
    print(f"lane {lane}: queue wait before encoder p50={pct(wait[lane],50):.3f} p95={pct(wait[lane],95):.3f} max={max(wait[lane]):.3f} s")
