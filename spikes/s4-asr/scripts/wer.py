#!/usr/bin/env python3
"""WER with Whisper's normalisers (case, punctuation, numbers, spelling).

  wer.py REF HYP [--lang en]      # each: a text file, or a TSV `id<TAB>text`
Prints: wer sub del ins n_ref_words [n_utts]. Reference tool only (Python venv).
"""
import argparse, json, sys
import jiwer
from whisper_normalizer.english import EnglishTextNormalizer
from whisper_normalizer.basic import BasicTextNormalizer


def load(p):
    lines = open(p, encoding="utf-8").read().splitlines()
    if lines and all("\t" in l for l in lines if l.strip()):
        return {k: v for k, v in (l.split("\t", 1) for l in lines if l.strip())}
    return {"_": " ".join(lines)}


ap = argparse.ArgumentParser()
ap.add_argument("ref"); ap.add_argument("hyp")
ap.add_argument("--lang", default="en")
ap.add_argument("--json", action="store_true")
a = ap.parse_args()
norm = EnglishTextNormalizer() if a.lang == "en" else BasicTextNormalizer()
ref, hyp = load(a.ref), load(a.hyp)
keys = [k for k in ref if k in hyp]
missing = [k for k in ref if k not in hyp]
R = [norm(ref[k]) for k in keys]
H = [norm(hyp[k]) for k in keys]
# jiwer rejects empty references
pairs = [(r, h) for r, h in zip(R, H) if r.strip()]
o = jiwer.process_words([p[0] for p in pairs], [p[1] if p[1].strip() else "<empty>" for p in pairs])
n = o.hits + o.substitutions + o.deletions
res = dict(wer=round(100 * o.wer, 2), sub=o.substitutions, dele=o.deletions, ins=o.insertions, n=n,
           utts=len(pairs), missing=len(missing))
print(json.dumps(res) if a.json else " ".join(f"{k}={v}" for k, v in res.items()))
