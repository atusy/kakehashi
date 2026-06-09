#!/usr/bin/env python3
"""Symbolicate and summarize a samply (Firefox-profiler-format) profile of the
kakehashi server, and emit collapsed stacks for flamegraph rendering.

samply on macOS symbolicates only when *serving* the profile in a browser; the
saved JSON keeps raw module-relative addresses. This script resolves the
kakehashi frames offline with `atos` against the binary's dSYM, so it works
headlessly and feeds inferno-flamegraph.

Usage:
    analyze.py PROFILE.json.gz \
        --dsym target/profiling/kakehashi.dSYM/Contents/Resources/DWARF/kakehashi \
        [--pid-name kakehashi] [--folded OUT.folded] [--top N]

Pipe the folded output to a flamegraph:
    analyze.py ... --folded /tmp/p.folded && inferno-flamegraph /tmp/p.folded > p.svg
"""
import argparse
import gzip
import json
import re
import subprocess
from collections import Counter

TEXT_BASE = 0x100000000  # __TEXT vmaddr for a macOS arm64/x86_64 executable


def load(path):
    if path.endswith(".gz"):
        with gzip.open(path) as f:
            return json.loads(f.read())
    with open(path, encoding="utf-8") as f:
        return json.loads(f.read())


def lib_name(libs, thread, func_idx):
    res = thread["resourceTable"]
    r = thread["funcTable"]["resource"][func_idx]
    if r is None or r < 0:
        return "?"
    rl = res.get("lib")
    if rl and r < len(rl) and rl[r] is not None and rl[r] >= 0:
        return libs[rl[r]].get("name", "?")
    return "?"


def symbolicate(dsym, arch, addrs):
    """Map kakehashi module-relative addresses -> 'func (file:line)' via atos."""
    if not addrs:
        return {}
    runtime = [hex(TEXT_BASE + a) for a in addrs]
    out = subprocess.run(
        ["atos", "-o", dsym, "-arch", arch, "-l", hex(TEXT_BASE)] + runtime,
        capture_output=True, text=True, check=True).stdout.splitlines()
    clean = {}
    for a, line in zip(addrs, out):
        s = re.sub(r"::h[0-9a-f]{16}", "", line)
        s = re.sub(r"\s*\(in kakehashi\)", "", s)
        clean[a] = s.strip()
    return clean


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("--dsym", required=True)
    ap.add_argument("--arch", default="arm64")
    ap.add_argument("--pid-name", default="kakehashi")
    ap.add_argument("--folded")
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()

    d = load(args.profile)
    libs = d["libs"]
    pids = {t["pid"] for t in d["threads"] if t.get("name") == args.pid_name} \
        or {t.get("pid") for t in d["threads"]}

    # Collect kakehashi addresses to symbolicate.
    kaddrs = set()
    for t in d["threads"]:
        if t["pid"] not in pids:
            continue
        ft = t["frameTable"]
        for fr in range(ft["length"]):
            if lib_name(libs, t, ft["func"][fr]) == "kakehashi":
                kaddrs.add(ft["address"][fr])
    sym = symbolicate(args.dsym, args.arch, sorted(kaddrs))

    def frame_label(t, fr):
        ft = t["frameTable"]
        lib = lib_name(libs, t, ft["func"][fr])
        if lib == "kakehashi":
            return sym.get(ft["address"][fr], hex(ft["address"][fr]))
        # Non-kakehashi: label by library so kernel/malloc/parser are visible.
        return f"[{lib}]"

    self_t, incl, folded = Counter(), Counter(), Counter()
    total = 0
    for t in d["threads"]:
        if t["pid"] not in pids:
            continue
        st = t["stackTable"]
        frame, prefix = st["frame"], st["prefix"]
        for s in t["samples"]["stack"]:
            if s is None:
                continue
            total += 1
            stack = []
            cur = s
            while cur is not None:
                stack.append(frame_label(t, frame[cur]))
                cur = prefix[cur]
            stack.reverse()
            leaf = stack[-1].split(" (")[0]
            self_t[leaf] += 1
            for f in set(x.split(" (")[0] for x in stack):
                incl[f] += 1
            folded[";".join(x.split(" (")[0] for x in stack)] += 1

    if total == 0:
        raise SystemExit("no samples for server pid")

    def line(n):
        return f"  {100.0 * n / total:5.1f}%  {n:6}"

    print(f"\n=== self-time (leaf) — {total} samples ===")
    for nm, n in self_t.most_common(args.top):
        print(f"{line(n)}  {nm}")
    print(f"\n=== inclusive (kakehashi semantic/query frames) — top {args.top} ===")
    rel = [(k, v) for k, v in incl.most_common()
           if k.startswith("kakehashi") or "regex" in k or "ts_query" in k
           or "ts_parser" in k]
    for nm, n in rel[:args.top]:
        print(f"{line(n)}  {nm}")

    if args.folded:
        with open(args.folded, "w", encoding="utf-8") as f:
            for stack, n in folded.items():
                f.write(f"{stack} {n}\n")
        print(f"\ncollapsed stacks -> {args.folded} ({len(folded)} unique)")


if __name__ == "__main__":
    main()
