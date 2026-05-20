#!/usr/bin/env bash
# Open the Firefox Profiler interactive view for a given profile mode.
#
# Records a fresh symbolicated samply profile of the `profile_matching`
# example for the given mode, then serves it via the Firefox Profiler web UI.
# Visit the URL it prints (usually http://127.0.0.1:3000) in any browser —
# you get the full Firefox Profiler interface: flamegraph, call tree,
# inverted call tree, timeline, search.
#
# Usage:
#   ./view-samply.sh <mode> [duration]
#
# Modes (same as the profile_matching binary):
#   rest-single  rest-spread  match-single  sweep-limit  sweep-market  cancel
#
# Env knobs:
#   DURATION   how long to record (default 20s; can also pass as 2nd arg)
#
# Requirements:
#   cargo install samply

set -euo pipefail

MODE=${1:-}
if [ -z "$MODE" ]; then
  echo "usage: $0 <mode> [duration]" >&2
  echo "  modes: rest-single rest-spread match-single sweep-limit sweep-market cancel" >&2
  exit 1
fi

DURATION="${2:-${DURATION:-20}}"

cd "$(dirname "$0")"
CRATE_ROOT="$(cd .. && pwd)"
ROOT="$(cd ../../../ && pwd)"
PROFILING="$CRATE_ROOT/profiling"
mkdir -p "$PROFILING"

PROFILE_JSON="$PROFILING/${MODE}.samply.json"

echo "→ building profile_matching (release)..."
(cd "$ROOT" && cargo build --release -p calvera-books --example profile_matching --quiet)

echo "→ recording ${MODE} for ${DURATION}s..."
samply record \
  --save-only \
  -o "$PROFILE_JSON" \
  "$ROOT/target/release/examples/profile_matching" "$MODE" "$DURATION"

echo ""
echo "→ serving via Firefox Profiler (Ctrl+C to stop)..."
echo "  profile: $PROFILE_JSON"
echo ""
exec samply load "$PROFILE_JSON"
