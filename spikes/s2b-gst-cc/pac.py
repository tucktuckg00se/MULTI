#!/usr/bin/env python3
"""Lists CEA-608 field-1 control codes (PAC row/indent, mode, tab) from a gst-cc.txt/ours dump."""
import sys
ROWS = {0x11: (1, 2), 0x12: (3, 4), 0x15: (5, 6), 0x16: (7, 8), 0x17: (9, 10), 0x10: (11, 11), 0x13: (12, 13), 0x14: (14, 15)}
MISC = {0x20: "RCL", 0x21: "BS", 0x24: "DER", 0x25: "RU2", 0x26: "RU3", 0x27: "RU4", 0x29: "RDC", 0x2c: "EDM", 0x2d: "CR", 0x2e: "ENM", 0x2f: "EOC"}
for line in open(sys.argv[1]):
    f = line.split()
    frame, codes = f[0], []
    for t in f[3:]:
        if not t.startswith(("fc", "f9")) or len(t) != 6:
            continue
        a, b = int(t[2:4], 16) & 0x7F, int(t[4:6], 16) & 0x7F
        if a in (0x14, 0x1C) and b in MISC:
            codes.append(MISC[b])
        elif a in (0x17, 0x1F) and 0x21 <= b <= 0x23:
            codes.append(f"TO{b - 0x20}")
        elif (a & 0x77) in ROWS and 0x40 <= b <= 0x7F:
            row = ROWS[a & 0x77][1 if b >= 0x60 else 0]
            ind = f" indent {((b & 0x0E) >> 1) * 4}" if b & 0x10 else ""
            codes.append(f"PAC row {row}{ind}")
        elif 0x11 <= a <= 0x13 or 0x19 <= a <= 0x1b:
            pass
    if codes:
        print(frame, ", ".join(codes))
