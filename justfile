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

# Run all benches. Pass `--quick` for a fast pass.
benches *FLAGS:
    @mkdir -p benches/logs
    @LOG="benches/logs/bench-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench hmap_book -- {{FLAGS}} 2>&1 | tee "$LOG"

# v2 benches: same workloads but matcher publishes fills into a Calvera
# Disruptor (cross-thread SPSC ring) instead of pushing into a Vec<Fill>.
# See benches/hmap_book_disruptor.rs.
benches-disruptor *FLAGS:
    @mkdir -p benches/logs
    @LOG="benches/logs/bench-disruptor-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench hmap_book_disruptor -- {{FLAGS}} 2>&1 | tee "$LOG"

# v3 benches: matcher buffers fills per-operation and emits them via one
# `batch_publish` at the end of each operation. Amortizes Disruptor slot-
# claim cost across all fills in an op. See benches/hmap_book_disruptor_batched.rs.
benches-disruptor-batched *FLAGS:
    @mkdir -p benches/logs
    @LOG="benches/logs/bench-disruptor-batched-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench hmap_book_disruptor_batched -- {{FLAGS}} 2>&1 | tee "$LOG"

# Run a single bench by name. Example: just bench-one limit_rest/single_level
bench-one NAME *FLAGS:
    @mkdir -p benches/logs
    @SAFE_NAME=$(echo "{{NAME}}" | tr '/' '_'); \
     LOG="benches/logs/bench-${SAFE_NAME}-$(date +%Y%m%d-%H%M%S).log"; \
     echo "→ logging to $LOG"; \
     cargo bench -p calvera-books --bench hmap_book -- {{NAME}} {{FLAGS}} 2>&1 | tee "$LOG"


# Capture all 6 flamegraphs
profile-all:
    ./bin/capture-flamegraphs.sh

# Open the Firefox Profiler interactive view for a single mode.
# Modes: rest-single, rest-spread, match-single, sweep-limit, sweep-market, cancel.
# Example: just profile-view rest-single 20
profile-view MODE DURATION="20":
    ./bin/view-samply.sh {{MODE}} {{DURATION}}

# Open a flamegraph SVG (from the latest run)
# Example: just flamegraph rest-single
flamegraph MODE:
    @LATEST=$(ls -t profiling/flamegraph/ 2>/dev/null | head -1); \
     if [ -z "$LATEST" ]; then \
        echo "no runs found — run \`just profile-all\` first"; exit 1; \
     fi; \
     echo "→ opening run $LATEST"; \
     open "profiling/flamegraph/$LATEST/flamegraph-{{MODE}}.svg"
