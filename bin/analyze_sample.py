#!/usr/bin/env python3
import re
import sys

counts = {}
with open("/tmp/led_sample.txt") as f:
    for line in f:
        m = re.search(r"(\d+)\s+(\S+)\s+\(in led\)", line)
        if m:
            n, fn = int(m.group(1)), m.group(2)
            counts[fn] = counts.get(fn, 0) + n

for fn, n in sorted(counts.items(), key=lambda x: -x[1])[:40]:
    print(f"{n:>6}  {fn}")
