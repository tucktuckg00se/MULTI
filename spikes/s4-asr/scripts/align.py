#!/usr/bin/env python3
"""Word-level reference for long.wav by CTC forced alignment of the known
transcript (reference tool only; runs in the Python venv, CPU).

  align.py long.wav long.segments.tsv > words.tsv     # word<TAB>start_s<TAB>end_s

Model: facebook/wav2vec2-base-960h (Apache-2.0). Each utterance from the
segments file is aligned on its own audio span (+-0.15 s) with a Viterbi pass
over the CTC emissions (blank = <pad>); a word spans its first to last char frame.
"""
import sys
import numpy as np
import soundfile as sf
import torch
from transformers import Wav2Vec2ForCTC, Wav2Vec2Processor

MODEL = "facebook/wav2vec2-base-960h"
REV = "22aad52d435eb6dbaf354bdad9b0da84ce7d6156"


def viterbi(lp, tokens, blank):
    """lp: [T, V] log-probs; tokens: label ids. Returns frame index per token."""
    T, N = lp.shape[0], len(tokens)
    S = 2 * N + 1
    ext = [blank if i % 2 == 0 else tokens[i // 2] for i in range(S)]
    neg = -1e30
    dp = np.full((T, S), neg)
    bp = np.zeros((T, S), dtype=np.int32)
    dp[0, 0] = lp[0, ext[0]]
    if S > 1:
        dp[0, 1] = lp[0, ext[1]]
    for t in range(1, T):
        for s in range(S):
            cands = [(dp[t - 1, s], s)]
            if s >= 1:
                cands.append((dp[t - 1, s - 1], s - 1))
            if s >= 2 and ext[s] != blank and ext[s] != ext[s - 2]:
                cands.append((dp[t - 1, s - 2], s - 2))
            v, b = max(cands)
            dp[t, s] = v + lp[t, ext[s]]
            bp[t, s] = b
    s = S - 1 if dp[T - 1, S - 1] >= dp[T - 1, S - 2] else S - 2
    path = [0] * T
    for t in range(T - 1, -1, -1):
        path[t] = s
        s = bp[t, s]
    first, last = [None] * N, [None] * N
    for t, s in enumerate(path):
        if s % 2 == 1:
            k = s // 2
            if first[k] is None:
                first[k] = t
            last[k] = t
    return first, last


def main():
    wav, segs = sys.argv[1], sys.argv[2]
    audio, sr = sf.read(wav, dtype="float32")
    assert sr == 16000
    proc = Wav2Vec2Processor.from_pretrained(MODEL, revision=REV)
    model = Wav2Vec2ForCTC.from_pretrained(MODEL, revision=REV).eval()
    vocab = proc.tokenizer.get_vocab()
    blank, sep = vocab["<pad>"], vocab["|"]
    print("word\tstart_s\tend_s")
    for line in open(segs):
        a, b, text = line.rstrip("\n").split("\t")
        a, b = float(a), float(b)
        s0 = max(0.0, a - 0.15)
        seg = audio[int(s0 * sr): int((b + 0.15) * sr)]
        x = proc(seg, sampling_rate=sr, return_tensors="pt").input_values
        with torch.no_grad():
            lp = torch.log_softmax(model(x).logits[0], -1).numpy()
        frame = len(seg) / sr / lp.shape[0]
        words = text.upper().split()
        toks, owner = [], []
        for wi, w in enumerate(words):
            for ch in w:
                if ch in vocab:
                    toks.append(vocab[ch]); owner.append(wi)
        first, last = viterbi(lp, toks, blank)
        for wi, w in enumerate(words):
            idx = [i for i, o in enumerate(owner) if o == wi]
            f = [first[i] for i in idx if first[i] is not None]
            l = [last[i] for i in idx if last[i] is not None]
            if not f:
                continue
            print(f"{w}\t{s0 + min(f) * frame:.3f}\t{s0 + (max(l) + 1) * frame:.3f}")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
