#!/usr/bin/env python3
"""Word timestamp accuracy: s6-e2e --words-log audio_ts vs the reference word
times. tsdemux shifts PTS by a per-run offset, so pass it:
shift = first video in_pts from s6.log ("new input session") - first raw PTS.
Usage: tsacc.py words.tsv long.words.tsv shift_s [origin_s=1.421333] [loop_s=485.645]"""
import re, sys
words, reff, shift = sys.argv[1], sys.argv[2], float(sys.argv[3])
origin = float(sys.argv[4]) if len(sys.argv) > 4 else 1.421333
loop = float(sys.argv[5]) if len(sys.argv) > 5 else 485.645
norm = lambda w: re.sub(r'[^a-z]', '', w.lower())
ref = [(norm(l.split('\t')[0]), float(l.split('\t')[1])) for l in open(reff).read().split('\n')[1:] if l]
d = []
for l in open(words).read().split('\n')[1:]:
    if not l: continue
    _, ts, _, t = l.split('\t'); n = norm(t)
    if not n: continue
    a = (float(ts) - shift - origin) % loop
    # nearest reference occurrence of the same word within 3 s
    c = [a - s for w, s in ref if w == n and abs(a - s) < 3]
    if c: d.append(min(c, key=abs))
d.sort(); p = lambda q: d[min(len(d) - 1, int(q * (len(d) - 1)))]
print(f"word ts - reference start (s): n={len(d)} p5={p(.05):.3f} p50={p(.5):.3f} p95={p(.95):.3f}")
