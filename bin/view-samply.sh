#!/usr/bin/env bash
# Open the Firefox Profiler interactive view for a given (workload × variant).
#
# Records a fresh symbolicated samply profile of the `profile_matching`
# example for the given combo, then serves it via the Firefox Profiler web UI.
# Visit the URL it prints (usually http://127.0.0.1:3000) in any browser —
# you get the full Firefox Profiler interface: flamegraph, call tree,
# inverted call tree, timeline, search.
#
# Usage:
#   ./view-samply.sh <workload> <variant> [duration]
#
# Workloads (same as benches/engine.rs):
#   mixed       add_cancel
# Variants:
#   v1          v2
#
# Env knobs:
#   DURATION   how long to record (default 20s; can also pass as 3rd arg)
#
# Requirements:
#   cargo install samply

set -euo pipefail

WORKLOAD=${1:-}
VARIANT=${2:-}
if [ -z "$WORKLOAD" ] || [ -z "$VARIANT" ]; then
  echo "usage: $0 <workload> <variant> [duration]" >&2
  echo "  workloads: mixed | add_cancel" >&2
  echo "  variants:  v1 | v2" >&2
  exit 1
fi

DURATION="${3:-${DURATION:-20}}"
TAG="${VARIANT}-${WORKLOAD}"

cd "$(dirname "$0")"
CRATE_ROOT="$(cd .. && pwd)"
ROOT="$(cd ../../../ && pwd)"
PROFILING="$CRATE_ROOT/profiling"
mkdir -p "$PROFILING"

PROFILE_JSON="$PROFILING/${TAG}.samply.json"

echo "→ building profile_matching (release)..."
(cd "$ROOT" && cargo build --release -p calvera-books --example profile_matching --quiet)

echo "→ recording ${TAG} for ${DURATION}s..."
samply record \
  --save-only \
  -o "$PROFILE_JSON" \
  "$ROOT/target/release/examples/profile_matching" "$WORKLOAD" "$VARIANT" "$DURATION"

echo ""
echo "→ serving via Firefox Profiler (Ctrl+C to stop)..."
echo "  profile: $PROFILE_JSON"
echo ""
exec samply load "$PROFILE_JSON"
