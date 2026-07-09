#!/usr/bin/env bash
#
# Profile the semanticTokens workload with Xcode Instruments' Time Profiler.
#
# `xctrace record --launch` profiles only the launched Python driver, not the
# child kakehashi server, so this records all processes for a bounded window
# while the driver runs and then extracts samples for target/profiling/kakehashi.
#
# Usage:
#   benches/profile/xctrace.sh [--lang rust|markdown] [--size N] [--requests N]
#                              [--edits N] [--time-limit 12s] [--file PATH]
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: xctrace profiling is macOS-only." >&2
  exit 1
fi

if [ -z "${DEVELOPER_DIR:-}" ] && [ -d /Applications/Xcode.app/Contents/Developer ]; then
  export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
fi

for tool in xctrace python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: required tool '$tool' not found." >&2
    exit 1
  }
done

LANG_ARG=markdown
SIZE=150
REQUESTS=160
EDITS=1
SETTLE=0.1
TIME_LIMIT=10s
FILE_ARG=

while [ $# -gt 0 ]; do
  case "$1" in
    --lang) LANG_ARG="$2"; shift 2;;
    --size) SIZE="$2"; shift 2;;
    --requests) REQUESTS="$2"; shift 2;;
    --edits) EDITS="$2"; shift 2;;
    --settle) SETTLE="$2"; shift 2;;
    --time-limit) TIME_LIMIT="$2"; shift 2;;
    --file) FILE_ARG="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 1;;
  esac
done

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BIN="target/profiling/kakehashi"
OUT="${TMPDIR:-/tmp}/kakehashi-xctrace"
TRACE="$OUT/semantic-time.trace"
TOC="$OUT/toc.xml"
XML="$OUT/time-profile.xml"
mkdir -p "$OUT"
rm -rf "$TRACE" "$TOC" "$XML"

echo "==> Building profiling binary (optimized + debug symbols)"
cargo build --profile profiling --bin kakehashi

FILE_OPTS=()
if [ -n "$FILE_ARG" ]; then
  FILE_OPTS=(--file "$FILE_ARG")
fi

echo "==> Recording all processes with xctrace (time-limit=$TIME_LIMIT)"
xctrace record --quiet --template "Time Profiler" --all-processes \
  --time-limit "$TIME_LIMIT" --output "$TRACE" &
TRACE_PID=$!

# Give Instruments a short head start so the target server spawn lands inside
# the trace window.
sleep 1
python3 benches/profile/drive.py \
  --bin "./$BIN" \
  --lang "$LANG_ARG" \
  --size "$SIZE" \
  --requests "$REQUESTS" \
  --edits "$EDITS" \
  --settle "$SETTLE" \
  "${FILE_OPTS[@]}"
wait "$TRACE_PID"

echo "==> Exporting trace tables"
xctrace export --input "$TRACE" --toc --output "$TOC" >/dev/null
xctrace export --input "$TRACE" \
  --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' \
  --output "$XML" >/dev/null

echo "==> Summarizing target/profiling/kakehashi samples"
python3 - "$XML" <<'PY'
import sys
import xml.etree.ElementTree as ET
from collections import Counter

root = ET.parse(sys.argv[1]).getroot()
process_fmt = {}
thread_fmt = {}
frame_name = {}
binary_path = {}
frame_binary = {}

for proc in root.iter("process"):
    ident = proc.attrib.get("id")
    if ident:
        process_fmt[ident] = proc.attrib.get("fmt", "")
for thread in root.iter("thread"):
    ident = thread.attrib.get("id")
    if ident:
        thread_fmt[ident] = thread.attrib.get("fmt", "")
for frame in root.iter("frame"):
    ident = frame.attrib.get("id")
    name = frame.attrib.get("name")
    if ident and name:
        frame_name[ident] = name
    binary = frame.find("binary")
    if ident and binary is not None:
        if "path" in binary.attrib:
            frame_binary[ident] = binary.attrib["path"]
        elif "ref" in binary.attrib:
            frame_binary[ident] = binary_path.get(binary.attrib["ref"], "")
    for binary in frame.findall("binary"):
        ident = binary.attrib.get("id")
        path = binary.attrib.get("path")
        if ident and path:
            binary_path[ident] = path

def resolve(elem, table):
    if elem is None:
        return ""
    if "fmt" in elem.attrib:
        return elem.attrib["fmt"]
    return table.get(elem.attrib.get("ref"), "")

def resolve_frame(frame):
    if "name" in frame.attrib:
        return frame.attrib["name"]
    return frame_name.get(frame.attrib.get("ref"), "<unresolved>")

def frame_path(frame):
    ident = frame.attrib.get("id") or frame.attrib.get("ref")
    return frame_binary.get(ident, "")

target_processes = set()
for row in root.iter("row"):
    process = resolve(row.find("process"), process_fmt)
    backtrace = row.find(".//backtrace")
    if backtrace is None:
        continue
    if any(
        frame_path(frame).endswith("/target/profiling/kakehashi")
        for frame in backtrace.findall("frame")
    ):
        target_processes.add(process)

leaf = Counter()
inclusive = Counter()
threads = Counter()
rows = 0
stacks = 0

for row in root.iter("row"):
    process = resolve(row.find("process"), process_fmt)
    if process not in target_processes:
        continue
    backtrace = row.find(".//backtrace")
    if backtrace is None:
        continue
    frames = [resolve_frame(frame) for frame in backtrace.findall("frame")]
    rows += 1
    threads[resolve(row.find("thread"), thread_fmt)] += 1
    if not frames:
        continue
    stacks += 1
    leaf[frames[0]] += 1
    for name in set(frames):
        inclusive[name] += 1

print(f"rows {rows} stacks {stacks}")
print()
print("threads")
for name, count in threads.most_common(12):
    print(f"{count:5d} {name}")
print()
print("leaf")
for name, count in leaf.most_common(25):
    print(f"{count:5d} {name}")
print()
print("inclusive kakehashi/tree-sitter/serde/hash")
terms = ("kakehashi::", "ts_", "tree_sitter::", "serde", "hashbrown", "hash::", "itoa")
shown = 0
for name, count in inclusive.most_common():
    if any(term in name for term in terms):
        print(f"{count:5d} {name}")
        shown += 1
        if shown >= 40:
            break
PY

echo "trace -> $TRACE"
echo "toc   -> $TOC"
echo "xml   -> $XML"
