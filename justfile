# By default just list all available commands
[private]
default:
    @just -l

# Lints the code
lint: clippy fmt-check doc-check

# Formats the code with nightly cargo
fmt:
    cargo +nightly fmt

# Checks that the code is formatted
fmt-check:
    cargo +nightly fmt -- --check

# Checks that docs emit no warnings
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --document-private-items --no-deps

# Checks clippy lints
clippy:
    cargo clippy --no-deps -- -D warnings

# Checks compilation
check:
    cargo check

alias b := build

# Builds in release mode
build:
    cargo build --release

alias t := test

# Runs the tests
test *FLAGS:
    cargo test {{FLAGS}}

# ---------------------------------------------------------------------------
# Benchmarks (criterion — see BENCHMARKS.md)
# ---------------------------------------------------------------------------

# Run the framework bench (one binary, every (impl × workload) combo).
# Pass `--quick` for a fast pass, or filter args like `v1/mixed`.
benches *FLAGS:
    @mkdir -p benches/logs
    @LOG="benches/logs/bench-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench engine -- {{FLAGS}} 2>&1 | tee "$LOG"

# Disruptor-batched bench (different consumer, separate bench binary).
# The framework's M4 consumer-axis sweep will eventually absorb this.
benches-disruptor-batched *FLAGS:
    @mkdir -p benches/logs
    @LOG="benches/logs/bench-disruptor-batched-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench orderbook_disruptor_batched -- {{FLAGS}} 2>&1 | tee "$LOG"

# Aggregate the most recent criterion results into one JSON + CSV under
# benches/logs/, tagged with host metadata. Run after `just benches`.
report:
    cargo run --release --quiet --example report

# Run a single bench id. Example: just bench-one v1/mixed
bench-one NAME *FLAGS:
    @mkdir -p benches/logs
    @SAFE_NAME=$(echo "{{NAME}}" | tr '/' '_'); \
     LOG="benches/logs/bench-${SAFE_NAME}-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench engine -- {{NAME}} {{FLAGS}} 2>&1 | tee "$LOG"


# Capture a flamegraph for every (workload × variant) combo in capture-flamegraphs.sh
profile-all:
    ./bin/capture-flamegraphs.sh

# Open the Firefox Profiler interactive view for a single (workload × variant).
# Workloads: mixed, add_cancel.  Variants: v1, v2.
# Example: just profile-view mixed v1 20
profile-view WORKLOAD VARIANT DURATION="20":
    ./bin/view-samply.sh {{WORKLOAD}} {{VARIANT}} {{DURATION}}

# Replay a Databento MBO tape (or a synthetic stand-in) against v1 and v2.
# Real tape:  just tape-replay --tape path/to/xnas-itch-20251110.mbo.dbn.zst
# Smoke:      just tape-replay --synthetic 200000
tape-replay *FLAGS:
    cargo run --release --quiet --example tape_replay -- {{FLAGS}}

# Open a flamegraph SVG (from the latest run). Tag format: <variant>-<workload>.
# Example: just flamegraph v1-mixed
flamegraph TAG:
    @LATEST=$(ls -t profiling/flamegraph/ 2>/dev/null | head -1); \
     if [ -z "$LATEST" ]; then \
        echo "no runs found — run \`just profile-all\` first"; exit 1; \
     fi; \
     echo "→ opening run $LATEST"; \
     open "profiling/flamegraph/$LATEST/flamegraph-{{TAG}}.svg"
