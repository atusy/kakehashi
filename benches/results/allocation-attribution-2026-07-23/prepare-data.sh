#!/bin/sh
# Install the parser/query data needed by this measurement.
set -eu

measurement_bin=${KAKEHASHI_MEASUREMENT_BIN:?set to the profiling binary built from the measured source}
data_dir=${KAKEHASHI_MEASUREMENT_DATA_DIR:?set to a disposable parser/query data directory}

[ -x "$measurement_bin" ] || {
  printf 'measurement binary is not executable: %s\n' "$measurement_bin" >&2
  exit 1
}

if [ -d "$data_dir" ] && [ -n "$(ls -A "$data_dir")" ]; then
  printf 'measurement data directory must be absent or empty: %s\n' "$data_dir" >&2
  exit 1
fi
mkdir -p "$data_dir"

# Use the measured kakehashi's public installer instead of cloning grammars,
# compiling parser libraries, or downloading query files in the evidence code.
for language in comment lua markdown markdown_inline python rust; do
  "$measurement_bin" --data-dir "$data_dir" language install "$language"
done

"$measurement_bin" --data-dir "$data_dir" language status
