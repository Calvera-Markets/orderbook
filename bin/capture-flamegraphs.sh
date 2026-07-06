#!/usr/bin/env bash
# Capture one flamegraph per (workload × variant) combo.
#
# Builds the `profile_matching` example, runs each combo for `DUR_RUN`
# seconds, samples it for `DUR_SAMPLE` seconds with macOS `sample`, and writes
# results into per-run subdirectories so each invocation is self-contained.
#
# Each `just profile-all` produces one RUN_ID = YYYYMMDD-HHMMSS, used as a
# subfolder under both flamegraph/ and logs/. Output layout:
#
#   crates/calvera-books/profiling/
#     flamegraph/<RUN_ID>/flamegraph-<variant>-<workload>.svg
#     logs/<RUN_ID>/<variant>-<workload>.log              ← throughput / stdout
#     logs/<RUN_ID>/<variant>-<workload>.sample.txt       ← Apple `sample` raw
#
# Combos map 1:1 to bench IDs in benches/engine.rs. Extend together with M3.3
# as new workloads are added.
#
# Env knobs:
#   DUR_RUN     wall-clock budget per combo (default 12s)
#   DUR_SAMPLE  how long `sample` captures per combo (default 8s)
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

# (workload, variant) combos, one per line as "workload:variant".
COMBOS=(
  "mixed:v1"
  "mixed:v2"
  "add_cancel:v1"
  "add_cancel:v2"
  "calm_market:v1"
  "calm_market:v2"
)

echo "→ building profile_matching (release)..."
(cd "$ROOT" && cargo build --release -p calvera-books --example profile_matching --quiet)
BIN="$ROOT/target/release/examples/profile_matching"

echo ""
echo "→ run-id: $RUN_ID"
echo "→ profiling ${#COMBOS[@]} combos (${DUR_RUN}s each, ${DUR_SAMPLE}s sampled)..."
SUMMARY=()

for COMBO in "${COMBOS[@]}"; do
  WORKLOAD="${COMBO%%:*}"
  VARIANT="${COMBO##*:}"
  TAG="${VARIANT}-${WORKLOAD}"
  echo ""
  echo "  [$TAG]"

  SAMPLE_TXT="$LOG_RUN_DIR/${TAG}.sample.txt"
  STDOUT_LOG="$LOG_RUN_DIR/${TAG}.log"
  SVG="$FLAMEGRAPH_RUN_DIR/flamegraph-${TAG}.svg"

  "$BIN" "$WORKLOAD" "$VARIANT" "$DUR_RUN" > "$STDOUT_LOG" 2>&1 &
  PID=$!
  sample "$PID" "$DUR_SAMPLE" -file "$SAMPLE_TXT" > /dev/null 2>&1
  wait "$PID"

  inferno-collapse-sample "$SAMPLE_TXT" \
    | inferno-flamegraph \
        --title "calvera-books: $TAG — run $RUN_ID (${DUR_RUN}s run, ${DUR_SAMPLE}s sampled)" \
    > "$SVG"

  SUMMARY_LINE=$(grep -E "^${VARIANT}/" "$STDOUT_LOG" || cat "$STDOUT_LOG")
  echo "  → $SVG"
  echo "  $SUMMARY_LINE"
  SUMMARY+=("$SUMMARY_LINE")
done

echo ""
echo "================================================================"
echo "done. run-id: $RUN_ID"
echo ""
echo "flamegraphs:"
for COMBO in "${COMBOS[@]}"; do
  WORKLOAD="${COMBO%%:*}"
  VARIANT="${COMBO##*:}"
  echo "  $FLAMEGRAPH_RUN_DIR/flamegraph-${VARIANT}-${WORKLOAD}.svg"
done
echo ""
echo "logs:"
echo "  $LOG_RUN_DIR/"
echo ""
echo "Run summary:"
for LINE in "${SUMMARY[@]}"; do
  echo "  $LINE"
done
