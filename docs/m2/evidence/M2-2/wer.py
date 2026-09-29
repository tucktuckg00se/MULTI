# Corpus WER of the source lane (clause log) vs MLS references; Whisper-style
# basic normalisation (lowercase, punctuation/symbols removed).
import re, sys, unicodedata
log, refs, lst = sys.argv[1:4]
def norm(t):
    t = unicodedata.normalize("NFKC", t).lower()
    t = "".join(" " if unicodedata.category(c)[0] in "PSC" else c for c in t)
    return t.split()
hyp = []
for line in open(log, errors="replace"):
    m = re.search(r'multi::run: clause id=\d+ text=(.*)$', line.rstrip())
    if m: hyp += norm(m.group(1))
ref_map = dict(l.rstrip("\n").split("\t", 1) for l in open(refs))
ref = []
for l in open(lst):
    ref += norm(ref_map[l.split("\t")[0]])
# word-level Levenshtein
prev = list(range(len(hyp) + 1))
for i, r in enumerate(ref, 1):
    cur = [i] + [0] * len(hyp)
    for j, h in enumerate(hyp, 1):
        cur[j] = min(prev[j] + 1, cur[j-1] + 1, prev[j-1] + (r != h))
    prev = cur
print(f"ref_words={len(ref)} hyp_words={len(hyp)} errors={prev[-1]} WER={100*prev[-1]/len(ref):.1f}%")
