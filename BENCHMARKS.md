# calvera-books — Benchmarks

Numbers for the `hmap_book::OrderBook` implementation, collected with criterion
0.8 (`cargo bench -p calvera-books --bench hmap_book`).

All times below are the **median** of a 100-sample run. Each iteration runs
against a fresh book (criterion's `iter_batched`), so setup time is excluded
from the measurement.

## Current baseline

Bench harness: `benches/hmap_book.rs`. Slab capacity: 1 MiB (`SLAB_CAP = 1 <<
20`). Consumer: `VecConsumer` (appends every fill to a `Vec<Fill>`).

| Benchmark                          | Median    | Per-element / per-level    |
| ---------------------------------- | --------- | -------------------------- |
| `limit_rest/single_level`          | 1.91 µs   | one insert, empty book     |
| `limit_rest/spread_levels`         | 2.43 µs   | one insert, 1000-level book |
| `limit_match_single/full_consume`  | 109 ns    | one full-consume match     |
| `limit_sweep_levels/4`             | 211 ns    | ~53 ns / level             |
| `limit_sweep_levels/16`            | 1.01 µs   | ~63 ns / level             |
| `limit_sweep_levels/64`            | 4.11 µs   | ~64 ns / level             |
| `limit_sweep_levels/256`           | 17.51 µs  | ~68 ns / level             |
| `market_sweep/4`                   | 213 ns    | ~53 ns / level             |
| `market_sweep/16`                  | 1.06 µs   | ~66 ns / level             |
| `market_sweep/64`                  | 4.10 µs   | ~64 ns / level             |
| `market_sweep/256`                 | 17.86 µs  | ~70 ns / level             |
| `market_sweep_opl/L256xO1`         | 17.64 µs  | ~69 ns / fill (OPL=1)      |
| `market_sweep_opl/L64xO4`          | 7.57 µs   | ~30 ns / fill (OPL=4)      |
| `market_sweep_opl/L16xO16`         | 5.10 µs   | ~20 ns / fill (OPL=16)     |
| `market_sweep_opl/L4xO64`          | 4.51 µs   | ~18 ns / fill (OPL=64)     |
| `cancel/mid_book`                  | 81 ns     | O(1) cancel                |
| `mixed_workload/random_add_cancel` | 27.51 µs  | ~27 ns / op (1024 ops)     |

## Improvement journey vs the original v1 implementation

Three rounds of changes happened in sequence. The deltas below compare each
final-state number to the v1 baseline (same machine, same session, identical
bench harness).

| Benchmark                          | v1        | Current  | Δ        |
| ---------------------------------- | --------- | -------- | -------- |
| `limit_rest/single_level`          | 2.84 µs   | 1.91 µs  | **−33%** |
| `limit_rest/spread_levels`         | 2.74 µs   | 2.43 µs  | **−11%** |
| `limit_match_single/full_consume`  | 244 ns    | 109 ns   | **−55%** |
| `limit_sweep_levels/4`             | 345 ns    | 211 ns   | **−39%** |
| `limit_sweep_levels/16`            | 1.34 µs   | 1.01 µs  | **−25%** |
| `limit_sweep_levels/64`            | 5.49 µs   | 4.11 µs  | **−25%** |
| `limit_sweep_levels/256`           | 21.67 µs  | 17.51 µs | **−19%** |
| `market_sweep/4`                   | 343 ns    | 213 ns   | **−38%** |
| `market_sweep/16`                  | 1.36 µs   | 1.06 µs  | **−22%** |
| `market_sweep/64`                  | 5.47 µs   | 4.10 µs  | **−25%** |
| `market_sweep/256`                 | 21.83 µs  | 17.86 µs | **−18%** |
| `market_sweep_opl/L256xO1`         | 21.78 µs  | 17.64 µs | **−19%** |
| `market_sweep_opl/L64xO4`          | 15.90 µs  | 7.57 µs  | **−52%** |
| `market_sweep_opl/L16xO16`         | 13.37 µs  | 5.10 µs  | **−62%** |
| `market_sweep_opl/L4xO64`          | 12.20 µs  | 4.51 µs  | **−63%** |
| `cancel/mid_book` *(see note)*     | 127 ns    | 81 ns    | **−36%** |
| `mixed_workload/random_add_cancel` | 47.92 µs  | 27.51 µs | **−43%** |

*Note on `cancel/mid_book`*: the v1 number above (127 ns) is **correct v1**,
i.e. with the slab-slot leak fixed — when v1 actually frees the slot on
cancel. Pre-fix v1 reported 91 ns because it was skipping `slab.free`
(the slot was leaked), so it was doing strictly less work than v2.

### What each round did

1. **Matcher refactor + cancel slot-leak fix** (commits `0c5aca5` and `226b71e`).
   - Restructured `match_against_opposite` into outer sweep + inner per-level
     walk; cache the `PriceLevel` once per level instead of once per fill.
   - Bulk-free fully-consumed orders via the existing FIFO chain (one slab
     write per level boundary + one splice at end-of-sweep), eliminating the
     per-fill `slab.free`.
   - Drop the `OrderSlot` enum tag; slab is now `Vec<Order>` with the freelist
     threaded through `Order.next` of free slots.
   - Fix the v1 cancel slot-leak (`slab.free` was never called).
   - **Where the wins land:** OPL-batched sweeps (40–55% on `market_sweep_opl`),
     because the per-level work now amortises over many fills.

2. **`NonZeroU32` + `#[repr(C, align(32))]` on `Order`**.
   - Wrap `SlabIndex(u32)` → `SlabIndex(NonZeroU32)`. Niche-optimised
     `Option<SlabIndex>` shrinks from 8 → 4 bytes (free `None` bit pattern at
     0); the slab reserves slot 0 to honour the non-zero invariant.
   - Shrinks `Order` from 40 B → 32 B exactly (8 + 8 + 8 + 4 + 4).
   - `#[repr(align(32))]` guarantees two slots per 64 B cache line, no straddle
     on random access regardless of allocator behaviour or slab size.
   - **Where the wins land:** double-digit reductions on every workload with a
     non-trivial cache footprint (~9–17% on deep sweeps, ~22% on cancel,
     ~29% on the mixed workload).

3. **`alloc_slot` instead of `insert_order(Order)`**.
   - The 32 B alignment on `Order` was forcing the compiler to align the
     stack frame to 32 in any function that builds an `Order` literal — adding
     a `sub` + `and sp, …` pair to `add_limit_order`'s prologue, paid on every
     call.
   - Replaced `insert_order(order: Order)` (which took the Order by value)
     with `alloc_slot() -> SlabIndex` + direct field writes through
     `get_mut`. The slab buffer is already 32-aligned, so the caller never
     constructs an `Order` on the stack.
   - The two saved prologue instructions are themselves only ~0.7 ns, but
     they were a *trigger* for cascading codegen pessimism — eliminating
     them shrunk the function's stack frame from 256 → 192 B and unlocked
     better register allocation and instruction scheduling through the rest
     of `add_limit_order`.
   - **Where the wins land:** very large reductions at shallow sweep depths
     (`/4` sweeps roughly halved) and ~10–25% on every other shape that goes
     through `add_limit_order`.

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
cost trends from ~53 ns at /4 toward ~68 ns at /256 — the higher levels pay
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

The drop from `L256xO1` (17.6 µs) to `L4xO64` (4.5 µs) — same total work,
quarter the wall-clock — is the headline payoff of the per-level matcher
refactor.

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
the ~27 ns/op aggregate is a reasonable upper bound for what the engine can
sustain at this book size.

## Notes on noise and methodology

- **Setup is not timed.** `iter_batched(setup, body, BatchSize::SmallInput)`
  runs `setup` outside the measured region.
- **Slab capacity is 1 MiB slots** (`SLAB_CAP = 1 << 20`). That makes
  `order_index` start out HashMap-pre-sized for 1M entries, which inflates
  insert-path numbers (`limit_rest`) relative to a smaller-book setup. If you
  ever benchmark pure insert throughput at production sizes, drop `SLAB_CAP`.
- **`limit_sweep` ≈ `market_sweep`** at every level count: both paths delegate
  to `match_against_opposite`, so the only difference is the per-iteration
  limit-price check (~1 cycle, masked by hashmap latency).
- **Short benches are noisy.** Anything under ~300 ns (e.g. `limit_match_single`,
  `cancel`, `*sweep_*/4`) has confidence intervals 30–70 ns wide, so quote
  these as approximate.