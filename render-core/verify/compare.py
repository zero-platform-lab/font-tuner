#!/usr/bin/env python3
"""Diff render-core's blend curves against the C++ oracle (bit-exact check)."""
import os, sys
tags = ["g125", "linear", "g13", "w115c14", "srgb", "mode2"]
ok = True
for t in tags:
    r = open(f"rust-{t}.txt").read().split()
    c = open(f"cpp-{t}.txt").read().split()
    d = [i for i, (a, b) in enumerate(zip(r, c)) if a != b]
    print(f"  gray {t:8s}: {'EXACT' if not d else str(len(d))+' diff'}")
    ok &= not d
if os.path.exists("lcd-rust.txt") and os.path.exists("lcd-cpp.txt"):
    r = open("lcd-rust.txt").read().split()
    c = open("lcd-cpp.txt").read().split()
    d = [i for i, (a, b) in enumerate(zip(r, c)) if a != b]
    print(f"  lcd  ({len(r)} vec): {'EXACT' if not d else str(len(d))+' diff'}")
    ok &= not d
print("RESULT:", "ALL BIT-EXACT" if ok else "MISMATCH")
sys.exit(0 if ok else 1)
