#!/usr/bin/env python3
"""Side-by-side sample of clause translations for human review.
  spotcheck.py RUNS_DIR OUT_FILE [systems...]"""
import json, os, sys
runs, out = sys.argv[1], sys.argv[2]
systems = sys.argv[3:] or ["opus", "m2m418", "madlad3b", "hymt18-q4", "qwen35-4b-q4", "gemma4-e4b-q4"]
ids = ["2", "6", "26", "52", "58", "60", "72", "119", "130", "144", "165", "196", "205", "218",
       "243", "247", "261", "273", "281", "283", "288", "289", "291", "298"]
def load(name):
    p = os.path.join(runs, name + ".jsonl")
    d = {}
    for l in open(p):
        r = json.loads(l)
        if r["pass"] == 0:
            d[(r["id"], r["lang"])] = r
    return d
ref = load("gemma4-26b-q4.ref")
data = {s: load(s + ".clauses") for s in systems}
with open(out, "w") as f:
    f.write("# S5 spot-check: %d clauses x es/fr/de/pt (+zh from the reference only). ref = gemma-4-26B-A4B pseudo-reference.\n" % len(ids))
    for i in ids:
        src = ref[(i, "es")]
        f.write(f"\n## {i} [{src['kind']}] {src['src']}\n")
        for lang in ["es", "fr", "de", "pt"]:
            f.write(f"  {lang} ref          | {ref[(i, lang)]['out']}\n")
            for s in systems:
                f.write(f"  {lang} {s:<13}| {data[s][(i, lang)]['out']}\n")
        f.write(f"  zh ref          | {ref[(i, 'zh')]['out']}\n")
