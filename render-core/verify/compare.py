#!/usr/bin/env python3
"""Diff render-core's blend curves against the C++ oracle.

render-core computes the blend in f32; the oracle is upstream's fixed-point
integer math. The formula is the same, so the two agree to within one 8-bit
level (float rounds to nearest where upstream's `>>16` truncates).
This checks that bound and reports the largest difference seen.
"""
import os
import sys

TOLERANCE = 1
tags = ["g125", "linear", "g13", "w115c14", "srgb", "mode2"]
worst = 0


def diff(rust, cpp):
    """Max absolute per-value difference between two whitespace-separated files."""
    r = [int(v) for v in open(rust).read().split()]
    c = [int(v) for v in open(cpp).read().split()]
    return max(abs(a - b) for a, b in zip(r, c))


for t in tags:
    d = diff(f"rust-{t}.txt", f"cpp-{t}.txt")
    worst = max(worst, d)
    print(f"  gray {t:8s}: max diff {d}")

if os.path.exists("lcd-rust.txt") and os.path.exists("lcd-cpp.txt"):
    r = [tuple(int(x) for x in v.split(",")) for v in open("lcd-rust.txt").read().split()]
    c = [tuple(int(x) for x in v.split(",")) for v in open("lcd-cpp.txt").read().split()]
    d = max(max(abs(a - b) for a, b in zip(x, y)) for x, y in zip(r, c))
    worst = max(worst, d)
    print(f"  lcd  ({len(r)} vec): max diff {d}")

ok = worst <= TOLERANCE
verdict = f"WITHIN +/-{TOLERANCE} (formula match)" if ok else "OUT OF TOLERANCE"
print(f"RESULT: max diff {worst} - {verdict}")
sys.exit(0 if ok else 1)
