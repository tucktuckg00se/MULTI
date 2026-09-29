#!/usr/bin/env python3
"""Caption lag on long.wav: per reference segment, the wall time at which the
last word of that segment was emitted, minus the end of its last spoken word
(forced alignment, long.words.tsv). feed.py adds a 1 s lead of silence.
lag.py words.tsv long.segments.tsv long.words.tsv"""
import sys
LEAD = 1.0
rows = [l.rstrip("\n").split("\t", 3) for l in open(sys.argv[1])]
rows = [(float(e), int(s) / 1000, int(en) / 1000) for e, s, en, _ in rows]
segs = [tuple(map(float, l.split("\t")[:2])) for l in open(sys.argv[2])]
words = [l.split("\t") for l in open(sys.argv[3]).read().splitlines()[1:]]
words = [(float(a), float(b)) for _, a, b in words]
lags = []
for s0, s1 in segs:
    ends = [b for a, b in words if s0 <= a < s1]
    if not ends:
        continue
    ref_end = max(ends) + LEAD
    hyp = [e for e, a, b in rows if s0 + LEAD <= a < s1 + LEAD]
    if hyp:
        lags.append(max(hyp) - ref_end)
lags.sort()
q = lambda p: lags[min(len(lags) - 1, int(p * len(lags)))]
print(f"segments={len(lags)}/{len(segs)} lag_p50={q(.5):.2f} p95={q(.95):.2f} max={lags[-1]:.2f}")
