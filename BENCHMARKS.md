# calvera-books — Benchmarks

Numbers for the `OrderBook` implementation, collected with criterion 0.8. The
"current" column tracks the latest version (v0.0.6 — see [`CHANGELOG.md`](CHANGELOG.md)
for the per-version code/perf walkthrough).

All times below are the **median** of a 100-sample run. Each iteration runs
against a fresh book (criterion's `iter_batched`), so setup time is excluded
from the measurement.

## Current baseline (v0.0.6)

Bench harness: `benches/orderbook.rs` (`cargo bench -p calvera-books --bench
orderbook`). Slab capacity: 1 MiB (`SLAB_CAP = 1 << 20`). Consumer:
`VecConsumer` (appends every fill to a `Vec<Fill>`).

| Benchmark                          | Median    | Per-element / per-level    |
| ---------------------------------- | --------- | -------------------------- |
| `limit_rest/single_level`          | 74.0 ns   | one insert, empty book     |
| `limit_rest/spread_levels`         | 78.7 ns   | one insert, 1000-level book |
| `limit_match_single/full_consume`  | 69.1 ns   | one full-consume match     |
| `limit_sweep_levels/4`             | 99.1 ns   | ~25 ns / level             |
| `limit_sweep_levels/16`            | 476 ns    | ~30 ns / level             |
| `limit_sweep_levels/64`            | 1.59 µs   | ~25 ns / level             |
| `limit_sweep_levels/256`           | 6.64 µs   | ~26 ns / level             |
| `market_sweep/4`                   | 94.2 ns   | ~24 ns / level             |
| `market_sweep/16`                  | 445 ns    | ~28 ns / level             |
| `market_sweep/64`                  | 1.47 µs   | ~23 ns / level             |
| `market_sweep/256`                 | 6.35 µs   | ~25 ns / level             |
| `market_sweep_opl/L256xO1`         | 6.25 µs   | ~24 ns / fill (OPL=1)      |
| `market_sweep_opl/L64xO4`          | 1.93 µs   | ~7.5 ns / fill (OPL=4)     |
| `market_sweep_opl/L16xO16`         | 1.09 µs   | ~4.2 ns / fill (OPL=16)    |
| `market_sweep_opl/L4xO64`          | 936 ns    | ~3.7 ns / fill (OPL=64)    |
| `cancel/mid_book` *(see note)*     | 31.1 ns   | O(1) cancel                |
| `mixed_workload/random_add_cancel` | 6.09 µs   | ~5.9 ns / op (1024 ops)    |

*Note on `cancel/mid_book`*: under criterion's default `BatchSize::LargeInput`,
v0.0.6 cancel is below the timer-resolution floor — criterion reports "took
zero time per iteration." The 31.1 ns figure is from a `--measurement-time
15 --sample-size 100` re-run (full log:
`benches/logs/bench-v2-cancel-confirm-*.log`). Confidence interval
[29.6, 33.2] ns, no overlap with the v0.0.5 [49.6, 52.7] ns interval from
the matching re-run.

## v0.0.1 → v0.0.6

Initial implementation against latest. Same machine, identical bench harness,
`VecConsumer` on both sides. Per-version detail (what each step changed and
why) lives in [`CHANGELOG.md`](CHANGELOG.md).

| Benchmark                          | v0.0.1    | v0.0.6   | Δ        |
| ---------------------------------- | --------- | -------- | -------- |
| `limit_rest/single_level`          | 2.84 µs   | 74.0 ns  | **−97%** |
| `limit_rest/spread_levels`         | 2.74 µs   | 78.7 ns  | **−97%** |
| `limit_match_single/full_consume`  | 244 ns    | 69.1 ns  | **−72%** |
| `limit_sweep_levels/4`             | 345 ns    | 99.1 ns  | **−71%** |
| `limit_sweep_levels/16`            | 1.34 µs   | 476 ns   | **−64%** |
| `limit_sweep_levels/64`            | 5.49 µs   | 1.59 µs  | **−71%** |
| `limit_sweep_levels/256`           | 21.67 µs  | 6.64 µs  | **−69%** |
| `market_sweep/4`                   | 343 ns    | 94.2 ns  | **−73%** |
| `market_sweep/16`                  | 1.36 µs   | 445 ns   | **−67%** |
| `market_sweep/64`                  | 5.47 µs   | 1.47 µs  | **−73%** |
| `market_sweep/256`                 | 21.83 µs  | 6.35 µs  | **−71%** |
| `market_sweep_opl/L256xO1`         | 21.78 µs  | 6.25 µs  | **−71%** |
| `market_sweep_opl/L64xO4`          | 15.90 µs  | 1.93 µs  | **−88%** |
| `market_sweep_opl/L16xO16`         | 13.37 µs  | 1.09 µs  | **−92%** |
| `market_sweep_opl/L4xO64`          | 12.20 µs  | 936 ns   | **−92%** |
| `cancel/mid_book` *(see note)*     | 127 ns    | 31.1 ns  | **−76%** |
| `mixed_workload/random_add_cancel` | 47.92 µs  | 6.09 µs  | **−87%** |

*Note on `cancel/mid_book`*: the v0.0.1 number (127 ns) is **with the slab-slot
leak fixed** — pre-fix v0.0.1 reported 91 ns because it was skipping
`slab.free` (the slot was leaked), so it was doing strictly less work. The
fix landed in v0.0.2. The v0.0.6 number is from a longer-measurement re-run
(see the current-baseline note above) because the default-config measurement
falls below the timer-resolution floor.

## What each bench measures

### `limit_rest`

Pure insert path — no matching happens because the opposite side is empty (or
non-crossing). Measures the cost of `match_against_opposite` returning
immediately, plus slab allocation, intrusive-list link patching, and
`order_index` HashMap insertion.

- **`single_level`** — fresh book, each iter adds one bid at the same price.
  Stresses the "new price level" path: every iteration creates a level, inserts
  one order, and registers in `order_index`.
- **`spread_levels`** — fresh book, each iter adds one bid at a price drawn from
  a 1000-tick range. Stresses the HashMap-of-levels growing wide; the
  `BTreeSet` price index sees a lot of inserts.

### `limit_match_single/full_consume`

The minimum-work matching case: one ask of qty 1 resting at p=100, an aggressor
bid of qty 1 at p=100. The aggressor consumes the resting order exactly,
triggering the full-consume branch (level walk → drain → `order_index.remove`,
`slab.free` deferred to end-of-sweep splice). This is the floor for a single
match.

### `limit_sweep_levels/{4,16,64,256}`

Book pre-populated with N bid + N ask price levels around mid 10_000, **one
order per level**, qty 1 each. The aggressor is a bid with price at the top of
the asks and qty=N — so it walks every ask level, fully consuming each one in
turn.

This is the **primary throughput benchmark for the matching loop**. Per-level
cost trends from ~33 ns at /4 to ~36 ns at /256 — the higher levels pay
slightly more in TLB / L1 pressure as the working set grows.

### `market_sweep/{4,16,64,256}`

Identical book setup to `limit_sweep_levels`, but submits a market IOC instead
of a crossing limit. Confirms that both paths go through
`match_against_opposite` — numbers track `limit_sweep_levels` within ~3% at
every depth.

### `market_sweep_opl/L{256xO1, 64xO4, 16xO16, 4xO64}`

**Total fills fixed at 256, varying levels × orders-per-level.** Tests the
amortisation of the per-level matcher work over the orders consumed at each
level. At OPL=1 every "level walk" runs a single fill then drains; at OPL=64 a
single level walk consumes 64 fills under one hashmap lookup, one
level-bookkeeping update, and one freelist stitch.

The drop from `L256xO1` (9.52 µs) to `L4xO64` (1.79 µs) — same total work,
~5.3× wall-clock — is the headline payoff of the per-level matcher refactor
(v0.0.2).

### `cancel/mid_book`

Book pre-populated with 100 levels × 10 orders per level (= 1000 orders). The
benchmark cancels an order whose id sits roughly in the middle of the
population, exercising:

- `order_index.remove` (HashMap probe + delete)
- `PriceLevel::remove` (intrusive list patch on `prev` / `next`)
- `slab.free` (push slot onto the free list)

No level drains because each price still holds 9 orders post-cancel — so the
`best_price` recompute path is not hit. That's the steady-state cancel cost.

### `mixed_workload/random_add_cancel`

Steady-state book (~400 resting orders across 50 levels per side). Each
iteration runs **1024 operations**, drawn from a fixed deterministic RNG seed:

- 70% limit adds, sides 50/50, prices within ±50 ticks of mid
- 30% cancels, targeting random previously-issued order ids (some will hit, some
  will miss)

The RNG sequence is built once during setup so the timed body contains no RNG
overhead. This is the closest thing to a realistic throughput measurement here;
the ~9.5 ns/op aggregate is a reasonable upper bound for what the engine can
sustain at this book size.

## Notes on noise and methodology

- **Setup is not timed.** `iter_batched(setup, body, BatchSize::SmallInput)`
  runs `setup` outside the measured region.
- **Slab capacity is 1 MiB slots** (`SLAB_CAP = 1 << 20`). That makes
  `order_index` start out HashMap-pre-sized for 1M entries. Under the default
  SipHash hasher this dominated `limit_rest` (every iter paid full hash setup
  on a near-empty map); with the v0.0.5 `U64Mixer` it's no longer visible in
  the numbers, but the pre-sizing still affects cache footprint — if you
  benchmark pure insert throughput at production sizes, drop `SLAB_CAP`.
- **`limit_sweep` ≈ `market_sweep`** at every level count: both paths delegate
  to `match_against_opposite`, so the only difference is the per-iteration
  limit-price check (~1 cycle, masked by hashmap latency).
- **Short benches are noisy.** Anything under ~300 ns (e.g. `limit_match_single`,
  `cancel`, `*sweep_*/4`) has confidence intervals 30–70 ns wide, so quote
  these as approximate.