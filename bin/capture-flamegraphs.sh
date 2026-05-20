#!/usr/bin/env bash
# Capture one flamegraph per matching-engine logic pathway.
#
# Builds the `profile_matching` example, runs each mode for `DUR_RUN`
# seconds, samples it for `DUR_SAMPLE` seconds with macOS `sample`, and writes
# results into per-run subdirectories so each invocation is self-contained.
#
# Each `just profile-all` produces one RUN_ID = YYYYMMDD-HHMMSS, used as a
# subfolder under both flamegraph/ and logs/. Output layout:
#
#   crates/calvera-books/profiling/
#     flamegraph/<RUN_ID>/flamegraph-<mode>.svg   ← what you open in a browser
#     logs/<RUN_ID>/<mode>.log                     ← throughput / stdout
#     logs/<RUN_ID>/<mode>.sample.txt              ← Apple `sample` raw output
#
# Modes map 1:1 to criterion benches (see BENCHMARKS.md).
#
# Env knobs:
#   DUR_RUN     wall-clock budget per mode (default 12s)
#   DUR_SAMPLE  how long `sample` captures per mode (default 8s)
#
# Requirements (install once):
#   cargo install inferno          # inferno-collapse-sample, inferno-flamegraph
#   `sample` is built into macOS.

set -euo pipefail

# bin/ → crate root → repo root → profiling output dir
cd "$(dirname "$0")"
CRATE_ROOT="$(cd .. && pwd)"
ROOT="$(cd ../../../ && pwd)"
PROFILING="$CRATE_ROOT/profiling"

RUN_ID="$(date +%Y%m%d-%H%M%S)"
FLAMEGRAPH_RUN_DIR="$PROFILING/flamegraph/$RUN_ID"
LOG_RUN_DIR="$PROFILING/logs/$RUN_ID"
mkdir -p "$FLAMEGRAPH_RUN_DIR" "$LOG_RUN_DIR"

DUR_RUN=${DUR_RUN:-12}
DUR_SAMPLE=${DUR_SAMPLE:-8}

MODES=(
  "rest-single"
  "rest-spread"
  "match-single"
  "sweep-limit"
  "sweep-market"
  "cancel"
)

echo "→ building profile_matching (release)..."
(cd "$ROOT" && cargo build --release -p calvera-books --example profile_matching --quiet)
BIN="$ROOT/target/release/examples/profile_matching"

echo ""
echo "→ run-id: $RUN_ID"
echo "→ profiling ${#MODES[@]} modes (${DUR_RUN}s each, ${DUR_SAMPLE}s sampled)..."
SUMMARY=()

for MODE in "${MODES[@]}"; do
  echo ""
  echo "  [$MODE]"

  SAMPLE_TXT="$LOG_RUN_DIR/${MODE}.sample.txt"
  STDOUT_LOG="$LOG_RUN_DIR/${MODE}.log"
  SVG="$FLAMEGRAPH_RUN_DIR/flamegraph-${MODE}.svg"

  "$BIN" "$MODE" "$DUR_RUN" > "$STDOUT_LOG" 2>&1 &
  PID=$!
  sample "$PID" "$DUR_SAMPLE" -file "$SAMPLE_TXT" > /dev/null 2>&1
  wait "$PID"

  inferno-collapse-sample "$SAMPLE_TXT" \
    | inferno-flamegraph \
        --title "calvera-books: $MODE — run $RUN_ID (${DUR_RUN}s run, ${DUR_SAMPLE}s sampled)" \
    > "$SVG"

  SUMMARY_LINE=$(grep -E "^${MODE}" "$STDOUT_LOG" || cat "$STDOUT_LOG")
  echo "  → $SVG"
  echo "  $SUMMARY_LINE"
  SUMMARY+=("$SUMMARY_LINE")
done

echo ""
echo "================================================================"
echo "done. run-id: $RUN_ID"
echo ""
echo "flamegraphs:"
for MODE in "${MODES[@]}"; do
  echo "  $FLAMEGRAPH_RUN_DIR/flamegraph-${MODE}.svg"
done
echo ""
echo "logs:"
echo "  $LOG_RUN_DIR/"
echo ""
echo "Run summary:"
for LINE in "${SUMMARY[@]}"; do
  echo "  $LINE"
done
