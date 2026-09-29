#!/usr/bin/env python3
"""WER of a feed.py words.tsv against refs. Usage:
wer.py words.tsv refs.tsv list.tsv   (MLS: refs id<TAB>text)
wer.py words.tsv --text long.txt      (single reference text)
Also prints lag: per word (emit - end_ms) and per clip (emit of last word ending
before clip end + 0.5 s, minus clip end), p50/p95."""
import sys, unicodedata, statistics
def norm(t):
    t = unicodedata.normalize("NFKC", t).lower()
    t = "".join(" " if unicodedata.category(c)[0] in "PSC" and c != "'" else c for c in t)
    return t.replace("'", "").split()
rows = [l.rstrip("\n").split("\t", 3) for l in open(sys.argv[1])]
rows = [(float(e), int(s), int(en), t) for e, s, en, t in rows if len(rows) and t]
hyp = [w for r in rows for w in norm(r[3])]
if sys.argv[2] == "--text":
    ref = norm(open(sys.argv[3]).read())
else:
    ref_map = dict(l.rstrip("\n").split("\t", 1) for l in open(sys.argv[2]))
    ref = [w for l in open(sys.argv[3]) for w in norm(ref_map[l.split("\t")[0]])]
prev = list(range(len(hyp) + 1)); ops = None
for i, r in enumerate(ref, 1):
    cur = [i] + [0] * len(hyp)
    for j, h in enumerate(hyp, 1):
        cur[j] = min(prev[j] + 1, cur[j-1] + 1, prev[j-1] + (r != h))
    prev = cur
def pct(v, q):
    v = sorted(v); return v[min(len(v)-1, int(q*len(v)))] if v else float("nan")
lag = [e - en/1000 for e, s, en, t in rows]
out = f"ref={len(ref)} hyp={len(hyp)} err={prev[-1]} WER={100*prev[-1]/max(1,len(ref)):.1f}% word_lag_p50={pct(lag,.5):.2f} p95={pct(lag,.95):.2f}"
try:
    clips = [l.rstrip("\n").split("\t") for l in open(sys.argv[1].rsplit(".",1)[0] + ".clips.tsv")]
    cl = []
    for cid, s0, s1 in clips:
        s0, s1 = float(s0), float(s1)
        ws = [r for r in rows if s0*1000 <= r[2] <= s1*1000 + 500]
        if ws: cl.append(max(r[0] for r in ws) - s1)
    if len(cl) > 1:
        out += f" clip_lag_p50={pct(cl,.5):.2f} p95={pct(cl,.95):.2f}"
except OSError:
    pass
print(out)
