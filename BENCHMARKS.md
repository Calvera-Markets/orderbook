# calvera-books — Benchmarks

Numbers for the `OrderBook` engine (per-side slab, side-packed handle,
const-generic matcher), collected with criterion 0.8 through
`benches/engine.rs`. See [`CHANGELOG.md`](CHANGELOG.md) for the optimization
history.

## Methodology — warm, steady-state measurement

The old harness built a fresh 1 MiB book per iteration (`iter_batched`); setup
was "untimed", but the **first touch** of those pages happened inside the timed
body — a one-off allocator/page-fault cost that isn't engine work and varies 4×
across page sizes (see [`lessons/pages.md`](lessons/pages.md)). The framework
fixes this:

- Each workload is a `(setup, hot)` pair. `setup` builds a pre-allocated,
  pre-populated book **once**; a warmup phase runs `hot` untimed to soft-prefault
  slab pages and warm HashMap buckets; only the steady-state `hot` loop is timed
  via criterion's `iter_custom`.
- Workloads hold the book at a bounded size (add/cancel balanced, or periodic
  refill), so an arbitrarily long timed run never drifts or exhausts the slab.

**Read a number as the cost of one `hot` iteration**, whose op composition
varies per workload (spelled out below) — it is *not* a uniform per-op figure.

### Running

```
cargo bench --bench engine
just bench-one mixed
just report                                 # → benches/logs/results-<ts>.{json,csv}
BENCH_SLAB_CAP=1<<18 cargo bench --bench engine
BENCH_SLAB_ALLOC=madvise cargo bench --bench engine  # Linux-only
```

Bench ids: `workload[/cap_<N>][/alloc_<slug>]`.

## Current results

Apple Silicon (aarch64), macOS; criterion 0.8, warm-up 1 s / measurement 3 s.
Each figure is the **mean of the medians of three runs**.

| Workload          | avg      | `hot` iteration                                   |
| ----------------- | -------: | ------------------------------------------------- |
| `mixed`           |  6.29 ns | one add *or* cancel (50/50), populated book        |
| `add_cancel`      | 11.02 ns | one add *or* cancel, alternating (book holds 0–1)  |
| `cancel_heavy`    |  9.82 ns | add-tail + cancel-head on a 50-deep single level   |
| `match_single`    | 21.51 ns | add-tail + full-consume match-head at one level    |
| `add_spread`      | 48.24 ns | add + cancel, each creating/draining a level (BTreeSet) |
| `sweep` *(noisy)* | 388.9 ns | one market order draining 8 levels (~50 ns/level)  |
| `deep_book`       | 52.79 µs | sweep + rebuild the ~5 K-order near band of a deep power-law book |
| `calm_market`     | 31.29 ns | one scenario event (OU + MM cancel/replace)        |
| `news_event`      | 31.76 ns | one scenario event (calm + rare Student-t jumps)   |
| `illiquid`        | 32.18 ns | one scenario event (wide spread, thin, aggressive) |
| `opening_auction` | 30.94 ns | one scenario event (deep resting book)             |

## What each workload measures

Micro-workloads isolate single code paths; scenario workloads (M6) replay
realistic event streams. Definitions live in `workloads.rs` /`scenarios.rs`
(single source of truth, shared by the bench runner, the profiler, and tests).

### Micro-workloads

- **`mixed`** — book pre-populated to ~400 resting orders (50 levels/side);
  1024 precomputed random ops at a 50/50 add:cancel ratio replayed with
  wrap-around, so the book stays bounded. The closest single-number "typical
  op" figure.
- **`add_cancel`** — tightest alloc/free loop: alternately add one order and
  cancel it. Book holds 0 or 1 orders; a floor for the allocation/index path.
- **`cancel_heavy`** — 50 orders pre-rested at one price; each iter appends at
  the tail and cancels the head. The level never drains, so this isolates the
  per-level FIFO unlink + slab reclaim (no BTreeSet churn).
- **`match_single`** — 16 asks pre-rested at one price; each iter adds one at the
  tail then crosses the head with a qty-1 bid (full consume). Add-before-consume
  keeps the level from draining, isolating the aggressor's full-consume match
  branch (freed-chain stitch, generation bump) plus one rest.
- **`add_spread`** — a FIFO of 128 orders cycling through 256 distinct prices;
  each iter adds at a fresh price (BTreeSet insert + new level) and cancels the
  oldest (BTreeSet remove + level drain). Stresses the price index.
- **`sweep`** — 512 single-order levels pre-rested; each iter's market order
  drains the 8 lowest (full consume + level removal + BTreeSet remove +
  best-price refresh, ×8). The book empties every 64 iters and the next iter
  refills all strips (a burst amortised over 64 iters). *Its refill burst gives
  it the widest confidence interval of the suite — quote it as approximate.*

#### `deep_book` — power-law depth profile

A large book with a realistic (Pareto-like) depth profile: per side,
`orders(d) = max(1, round(1000 / d))` orders rest at each of 256 price levels
`d` ticks off the mid — densest at the BBO, thinning outward. Liquidity is
modelled as order *count* per level (the matcher-relevant dimension), so **~81%
of a side's ~6.1 K orders sit in the near 80 levels**; the two-sided resting set
is ~382 KiB, past L1. Both sides are populated with allocations interleaved;
the per-side slab packs same-side orders onto the same cache lines.

Each hot iter is one market order that consumes exactly the near-mid band (~5 K
orders — a long same-side chain walk), followed by rebuilding that band.

### Scenario workloads (M6)

Each replays an OU-mid + market-maker event stream (`scenarios.rs`), deterministic
in a pinned `(params, seed)` and byte-stable via `rand_chacha`.

- **`calm_market`** — low-vol OU, MM-dominated, no jumps, ~1% aggressor rate.
  The baseline cancel/replace regime.
- **`news_event`** — calm baseline punctuated by rare fat-tailed (Student-t)
  jumps that drag the mid, spreading quotes to far prices. Per-op cost ≈
  `calm_market` — jumps are rare and the engine absorbs them cheaply (a finding,
  not a defect).
- **`illiquid`** — wide spread, thin book (mm_depth 4), frequent aggressors.
  The most matching-heavy scenario, hence the highest per-event cost.
- **`opening_auction`** — deep resting book (mm_depth 50). Stresses the slab and
  index at higher occupancy.

## Tape replay (`just tape-replay`)

10 M synthetic A/C/M ops, ~4 k live book. Apple Silicon, 2026-08-17.
`--no-latency` for throughput. ordertruques (real NVDA tape, Ryzen): 14.37 M/s.

| M ops/s | ns/op | P99 | P99.9 |
| ------: | ----: | --: | ----: |
|   36.33 |  27.5 | 84 ns | 208 ns |

## Notes on noise and methodology

- **Only the steady-state loop is timed.** Allocation, first-touch, population,
  and warmup all happen before `iter_custom`'s measured region.
- **Slab capacity** defaults per workload (16 K for populated workloads, 1 K for
  the small ones) rather than 1 MiB, keeping first-touch cost and cache
  footprint honest. Override with `BENCH_SLAB_CAP`.
- **Allocator axis** (`BENCH_SLAB_ALLOC=system|madvise|hugetlb`) is wired but the
  huge-page variants are Linux-only; on macOS they skip with a notice. The
  cross-platform page-fault comparison is pending a Linux host.
- **Run-to-run noise (observed over 3 runs).** The sub-15 ns micro-workloads
  (`mixed`, `add_cancel`, `cancel_heavy`) drift ~1–5%; the ~20–35 ns workloads
  (`match_single`, scenarios) ~2–7%; `sweep` is the outlier at ~3–13% because of
  its every-64-iters refill burst. The table above averages three runs to damp
  this. On a quiet, pinned machine the spreads would be tighter.
- **`sweep` — quote loosely.** Its refill burst dominates variance; the 3-run
  average (~389–396 ns) is a better estimate than any single median.
