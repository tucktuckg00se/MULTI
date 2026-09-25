#!/usr/bin/env python3
"""Exact-text check: every expected line must appear as a decoded row.

compare.py DIR EXPECTED.txt [NAME...]  (NAME default: gst ours)
For each decoder SRT in DIR, prints per expected line "ok" or the closest row.
ccextractor 708 output is raw Latin-1 (S3 gotcha), so it is read as Latin-1.
"""
import difflib, glob, os, re, sys

d, exp = sys.argv[1], sys.argv[2]
names = sys.argv[3:] or ["gst", "ours"]
expected = [l.rstrip("\n") for l in open(exp, encoding="utf-8") if l.strip()]

def rows(path):
    enc = "latin-1" if "-708" in path else "utf-8"
    txt = open(path, encoding=enc, errors="replace").read().replace("\r", "")
    out = []
    for l in txt.split("\n"):
        if not l.strip() or "-->" in l or re.fullmatch(r"\d+", l.strip()):
            continue
        l = re.sub(r"<[^>]*>", "", l)          # ffmpeg/ccx font tags
        l = re.sub(r"\{\\an\d\}", "", l).replace("\\h", " ")
        out.append(l.strip())
    return out

total = {}
for n in names:
    for f in sorted(glob.glob(f"{d}/{n}-*.srt")):
        rs = rows(f)
        if not rs:
            continue
        ok = 0
        lines = []
        for e in expected:
            if e in rs:
                ok += 1
                lines.append(f"  ok   {e}")
            else:
                best = difflib.get_close_matches(e, rs, n=1, cutoff=0.3)
                lines.append(f"  DIFF {e!r}\n       got {best[0]!r}" if best else f"  MISS {e!r}")
        print(f"== {os.path.basename(f)}: {ok}/{len(expected)} exact")
        print("\n".join(lines))
