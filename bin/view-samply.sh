#!/usr/bin/env bash
# Open the Firefox Profiler interactive view for a workload.
#
# Usage:
#   ./view-samply.sh <workload> [duration]
#
# Env knobs:
#   DURATION   how long to record (default 20s; can also pass as 2nd arg)
#
# Requirements:
#   cargo install samply

set -euo pipefail

WORKLOAD=${1:-}
if [ -z "$WORKLOAD" ]; then
  echo "usage: $0 <workload> [duration]" >&2
  echo "  workloads: mixed | add_cancel | ..." >&2
  exit 1
fi

DURATION="${2:-${DURATION:-20}}"
TAG="$WORKLOAD"

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
  "$ROOT/target/release/examples/profile_matching" "$WORKLOAD" "$DURATION"

echo ""
echo "→ serving via Firefox Profiler (Ctrl+C to stop)..."
echo "  profile: $PROFILE_JSON"
echo ""
exec samply load "$PROFILE_JSON"
