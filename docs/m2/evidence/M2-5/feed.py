#!/usr/bin/env python3
"""Feed WAV clips (16 kHz mono s16) to a MULTI worker over the stdio protocol.

feed.py --list list.tsv|--wav f.wav [--gap 1.0] [--rate 1.0|0] --out words.tsv -- <worker cmd...>
Writes: emit_s  start_ms  end_ms  text   (emit_s = wall time since first frame)
and clips.tsv next to --out: id  start_s  end_s  (timeline of the fed audio).
"""
import argparse, json, struct, subprocess, sys, threading, time, wave

ap = argparse.ArgumentParser()
ap.add_argument("--list"); ap.add_argument("--wav")
ap.add_argument("--gap", type=float, default=1.0)
ap.add_argument("--lead", type=float, default=1.0)
ap.add_argument("--rate", type=float, default=1.0)
ap.add_argument("--out", required=True)
ap.add_argument("cmd", nargs=argparse.REMAINDER)
a = ap.parse_args()
cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd

clips = []
if a.list:
    for l in open(a.list):
        i, p = l.rstrip("\n").split("\t")[:2]
        clips.append((i, p))
else:
    clips.append(("wav", a.wav))

pcm = bytearray(b"\0\0" * int(16000 * a.lead))
bounds = []
for cid, p in clips:
    w = wave.open(p)
    assert w.getframerate() == 16000 and w.getnchannels() == 1 and w.getsampwidth() == 2, p
    s0 = len(pcm) // 2
    pcm += w.readframes(w.getnframes())
    bounds.append((cid, s0 / 16000, len(pcm) // 2 / 16000))
    pcm += b"\0\0" * int(16000 * a.gap)
pcm += b"\0\0" * 16000 * 2

proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
out = open(a.out, "w")
t_start = [None]
ready = threading.Event()

def reader():
    f = proc.stdout
    while True:
        h = f.read(4)
        if len(h) < 4:
            break
        n = struct.unpack("<I", h)[0]
        buf = f.read(n)
        if buf[0] != 1:
            continue
        m = json.loads(buf[1:])
        now = time.monotonic()
        if m.get("type") == "ready" or "Ready" in m or m.get("kind") == "ready":
            ready.set()
        ws = m.get("words")
        if ws is None and isinstance(m.get("Words"), dict):
            ws = m["Words"].get("words")
        if ws:
            e = now - (t_start[0] or now)
            for w in ws:
                out.write(f"{e:.3f}\t{w['start_ms']}\t{w['end_ms']}\t{w['text']}\n")
            out.flush()
        elif not ready.is_set():
            ready.set()  # first message is Ready
        if "error" in json.dumps(m).lower():
            print("worker:", m, file=sys.stderr)

th = threading.Thread(target=reader, daemon=True); th.start()
ready.wait(120)

def frame(kind, payload):
    return struct.pack("<I", len(payload) + 1) + bytes([kind]) + payload

FR = 320  # 20 ms
t_start[0] = time.monotonic()
for k, off in enumerate(range(0, len(pcm), FR * 2)):
    chunk = pcm[off:off + FR * 2]
    start_ms = k * 20
    proc.stdin.write(frame(2, struct.pack("<Q", start_ms) + bytes(chunk)))
    if a.rate > 0:
        due = t_start[0] + (k + 1) * 0.02 / a.rate
        d = due - time.monotonic()
        if d > 0:
            time.sleep(d)
proc.stdin.write(frame(1, json.dumps({"type": "shutdown"}).encode()))
proc.stdin.close()
proc.wait()
th.join(5)
out.close()
with open(a.out.rsplit(".", 1)[0] + ".clips.tsv", "w") as f:
    for cid, s0, s1 in bounds:
        f.write(f"{cid}\t{s0:.3f}\t{s1:.3f}\n")
print(f"fed {len(pcm)/32000:.1f}s in {time.monotonic()-t_start[0]:.1f}s", file=sys.stderr)
