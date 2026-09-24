#!/usr/bin/env python3
"""Scores S5 runs. Reference tooling only (the measured system is the Rust binary).

  score.py RUNS_DIR OUT_DIR [--comet]

- *.flores*.jsonl: chrF++ (sacrebleu) and optional COMET (Unbabel/wmt22-comet-da)
  against FLORES-200 devtest references.
- *.clauses*.jsonl: same metrics against pseudo-references made by a much larger
  model (gemma4-26b-q4.ref.jsonl), plus failure-mode heuristics.
- Latency per run from the JSONL (P50/P95/max per language and whole request).

Writes OUT_DIR/results.csv and OUT_DIR/failures.csv. COMET needs a GPU; run it
under the GPU lock.
"""
import csv
import glob
import json
import os
import re
import statistics
import sys
from collections import defaultdict

import sacrebleu

RUNS, OUT = sys.argv[1], sys.argv[2]
USE_COMET = "--comet" in sys.argv
MEDIA = os.path.dirname(os.path.abspath(RUNS))
REF_RUN = "gemma4-26b-q4.ref"


def load(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def pct(v, p):
    if not v:
        return float("nan")
    v = sorted(v)
    return v[min(len(v) - 1, round(p / 100 * (len(v) - 1)))]


flores_ref = {}
for lang in ["es", "fr", "de", "pt", "zh"]:
    p = os.path.join(MEDIA, f"flores.ref.{lang}.txt")
    if os.path.exists(p):
        flores_ref[lang] = [l.rstrip("\n") for l in open(p)]

clause_ref = {}
p = os.path.join(RUNS, REF_RUN + ".jsonl")
if os.path.exists(p):
    for r in load(p):
        if r["pass"] == 0:
            clause_ref[(r["id"], r["lang"])] = r["out"]

comet_model = None
if USE_COMET:
    from comet import load_from_checkpoint

    ck = glob.glob(os.path.expanduser(
        "~/.cache/multi-models/comet/wmt22-comet-da/**/model.ckpt"), recursive=True)[0]
    comet_model = load_from_checkpoint(ck)


def comet(src, hyp, ref):
    if comet_model is None:
        return float("nan")
    data = [{"src": s, "mt": h, "ref": r} for s, h, r in zip(src, hyp, ref)]
    out = comet_model.predict(data, batch_size=64, gpus=1, progress_bar=False)
    return out.system_score * 100


def words(s):
    return len(s.split())


def failure(r, ref):
    """Heuristic failure label for one clause translation, or ''."""
    out, src = r["out"].strip(), r["src"].strip()
    if not out:
        return "empty"
    if r.get("cap"):
        return "capped:" + r["cap"]
    if "\n" in out or re.search(r"(?i)\b(translation|translated|note:|here is|nota:|traducción|traduction|übersetzung|tradução)\b", out):
        return "commentary"
    if r["lang"] != "zh" and out.lower() == src.lower() and words(src) > 2:
        return "untranslated"
    if r["lang"] != "zh" and sacrebleu.sentence_chrf(out, [src]).score > 75 and words(src) > 3:
        return "mostly-untranslated"
    ratio = words(out) / max(1, words(src))
    if r["lang"] != "zh" and ratio > 2.2 and words(src) >= 3:
        return "too-long"
    if r["kind"] == "fragment" and ref and words(out) > words(ref) + 3:
        return "fragment-continued"
    return ""


rows, fails = [], []
for path in sorted(glob.glob(os.path.join(RUNS, "*.jsonl"))):
    name = os.path.basename(path)[:-6]
    if name == "summaries" or name.endswith(".soak") or name.endswith(".soak+asr"):
        continue
    recs = [r for r in load(path) if r["pass"] == 0]
    if not recs:
        continue
    summ = {}
    sp = os.path.join(RUNS, name + ".summary.json")
    if os.path.exists(sp):
        try:
            summ = json.load(open(sp))
        except json.JSONDecodeError:
            pass
    by_lang = defaultdict(list)
    for r in recs:
        by_lang[r["lang"]].append(r)
    req = {}
    for r in recs:
        req[r["id"]] = r["req_ms"]
    reqv = list(req.values())
    base = {
        "run": name,
        "n": len(req),
        "load_ms": round(summ.get("load_ms", float("nan")), 0),
        "vram_mib": summ.get("vram_mib"),
        "req_p50": round(pct(reqv, 50), 1),
        "req_p95": round(pct(reqv, 95), 1),
        "req_max": round(pct(reqv, 100), 1),
        "caps": sum(1 for r in recs if r.get("cap")),
    }
    for lang, rs in sorted(by_lang.items()):
        ms = [r["ms"] for r in rs]
        ttft = [r["ttft_ms"] for r in rs if r.get("ttft_ms") is not None]
        row = dict(base, lang=lang,
                   p50=round(pct(ms, 50), 1), p95=round(pct(ms, 95), 1), max=round(pct(ms, 100), 1),
                   ttft_p50=round(pct(ttft, 50), 1) if ttft else "",
                   out_tok_per_s=round(sum(r["n_out"] for r in rs) / (sum(ms) / 1000), 0) if any(r["n_out"] for r in rs) else "")
        hyp = [r["out"] for r in rs]
        src = [r["src"] for r in rs]
        tok = "zh" if lang == "zh" else "13a"
        if ".flores" in name and lang in flores_ref:
            ref = [flores_ref[lang][int(r["id"]) - 1] for r in rs]
            row["ref"] = "flores"
        elif ".clauses" in name or name.endswith(".ref"):
            if name == REF_RUN:
                ref = None
            else:
                ref = [clause_ref.get((r["id"], lang), "") for r in rs]
            row["ref"] = "pseudo(gemma4-26b)"
        else:
            ref = None
        if ref:
            row["chrf"] = round(sacrebleu.corpus_chrf(hyp, [ref], word_order=2).score, 1)
            row["comet"] = round(comet(src, hyp, ref), 1) if USE_COMET else ""
        if ".clauses" in name:
            f = defaultdict(int)
            for r in rs:
                ref1 = clause_ref.get((r["id"], lang), "")
                lab = failure(r, ref1)
                if lab:
                    f[lab] += 1
                    fails.append({"run": name, "id": r["id"], "kind": r["kind"], "lang": lang,
                                  "label": lab, "src": r["src"], "out": r["out"], "ref": ref1})
            row["failures"] = ";".join(f"{k}={v}" for k, v in sorted(f.items()))
        rows.append(row)
        print(name, lang, row.get("chrf"), row.get("comet"), row["p50"], row["p95"], file=sys.stderr)

os.makedirs(OUT, exist_ok=True)
cols = ["run", "lang", "n", "ref", "chrf", "comet", "p50", "p95", "max", "ttft_p50", "req_p50", "req_p95",
        "req_max", "out_tok_per_s", "load_ms", "vram_mib", "caps", "failures"]
with open(os.path.join(OUT, "results.csv"), "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=cols, extrasaction="ignore")
    w.writeheader()
    w.writerows(rows)
with open(os.path.join(OUT, "failures.csv"), "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=["run", "id", "kind", "lang", "label", "src", "out", "ref"])
    w.writeheader()
    w.writerows(fails)
