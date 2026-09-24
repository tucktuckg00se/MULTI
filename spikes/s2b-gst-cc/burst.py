#!/usr/bin/env python3
"""Per-line on-air lag for burst scenarios.

burst.py DIR [NAME...]: lines are "<P><kk> QUICK BROWN FOX JUMPS OVER", pushed
at t = 1 + kk/3 s. For each decoded SRT, reports how many lines appear exactly,
and the lag (first cue start showing the complete line minus push time).
"""
import glob, os, re, statistics, sys

d = sys.argv[1]
names = sys.argv[2:] or ["gst", "ours"]
def ts(s):
    h, m, rest = s.split(":"); sec, ms = rest.replace(".", ",").split(",")
    return int(h) * 3600 + int(m) * 60 + int(sec) + int(ms) / 1000
for n in names:
    for f in sorted(glob.glob(f"{d}/{n}-ccx-*.srt")) + sorted(glob.glob(f"{d}/{n}-libcaption.srt")):
        enc = "latin-1" if "-708" in f else "utf-8"
        txt = open(f, encoding=enc, errors="replace").read().replace("\r", "")
        first = {}
        for block in txt.split("\n\n"):
            ls = block.strip().split("\n")
            tl = [l for l in ls if "-->" in l]
            if not tl:
                continue
            start = ts(tl[0].split("-->")[0].strip())
            for l in ls:
                for m in re.finditer(r"([AB])(\d\d) QUICK BROWN FOX JUMPS OVER", l):
                    key = m.group(1) + m.group(2)
                    first.setdefault(key, start)
        by_p = {}
        for key, t in first.items():
            by_p.setdefault(key[0], []).append((int(key[1:]), t - (1 + int(key[1:]) / 3)))
        for p, v in sorted(by_p.items()):
            v.sort()
            lags = [x[1] for x in v]
            missing = sorted(set(range(90)) - {x[0] for x in v})
            print(f"{os.path.basename(f)} lane {p}: {len(v)}/90 lines exact; lag p50 {statistics.median(lags):.2f} s, max {max(lags):.2f} s; "
                  f"last line on air at {max(x[1] + 1 + x[0] / 3 for x in v):.1f} s; missing {len(missing)}"
                  + (f" (first {missing[:6]})" if missing else ""))
