#!/usr/bin/env python3
"""ffmpeg's ccaption_dec with -real_time 1 emits a cue per caption change (true
appear times) but with bogus end times; set each end to the next cue's start.
Usage: rtfix.py in.srt out.srt"""
import re, sys
blocks = [b for b in re.split(r'\r?\n\r?\n', open(sys.argv[1], encoding='utf-8', errors='replace').read()) if '-->' in b]
starts = [re.search(r'(\S+) -->', b).group(1) for b in blocks]
with open(sys.argv[2], 'w') as f:
    for i, b in enumerate(blocks):
        lines = b.strip().split('\n'); k = next(j for j, l in enumerate(lines) if '-->' in l)
        end = starts[i + 1] if i + 1 < len(starts) else starts[i]
        f.write(f"{i + 1}\n{starts[i]} --> {end}\n" + '\n'.join(lines[k + 1:]) + '\n\n')
