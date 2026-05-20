# calvera-books — Benchmarks

Baseline numbers for the `hmap_book::OrderBook` implementation, collected with
criterion 0.8 (`cargo bench -p calvera-books`).

All times below are the **median** of a 100-sample run. Each iteration runs
against a fresh book (criterion's `iter_batched`), so setup time is excluded
from the measurement.

| Benchmark                          | Median time | Per-element / per-level    |
| ---------------------------------- | ----------- | -------------------------- |
| `limit_rest/single_level`          | 4.93 µs     | one insert, empty book     |
| `limit_rest/spread_levels`         | 2.67 µs     | one insert, 1000-level book |
| `limit_match_single/full_consume`  | 344 ns      | one full-consume match     |
| `limit_sweep_levels/4`             | 292 ns      | ~73 ns / level             |
| `limit_sweep_levels/16`            | 1.59 µs     | ~99 ns / level             |
| `limit_sweep_levels/64`            | 6.00 µs     | ~94 ns / level             |
| `limit_sweep_levels/256`           | 21.45 µs    | ~84 ns / level             |
| `market_sweep/4`                   | 363 ns      | ~91 ns / level             |
| `market_sweep/16`                  | 1.59 µs    | ~100 ns / level            |
| `market_sweep/64`                  | 6.04 µs     | ~94 ns / level             |
| `market_sweep/256`                 | 23.37 µs    | ~91 ns / level             |
| `cancel/mid_book`                  | 120 ns      | O(1) cancel                |
| `mixed_workload/random_add_cancel` | 78.37 µs    | ~77 ns / op (1024 ops)     |

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
triggering the full-consume branch (`pop_order_from_l2_book`, level deletion,
`order_index` removal, `slab.free`). This is the floor for a single match.

### `limit_sweep_levels/{4,16,64,256}`

Book pre-populated with N bid + N ask price levels around mid 10_000, one order
per level, qty 1 each. The aggressor is a bid with price at the top of the asks
and qty=N — so it walks every ask level, fully consuming each one in turn.

This is the **primary throughput benchmark for the matching loop**. Per-level
cost (~90 ns) is what the `[HalfBook; 2]` / branchless side-dispatch refactor
would need to beat.

### `market_sweep/{4,16,64,256}`

Identical book setup to `limit_sweep_levels`, but submits a market IOC instead
of a crossing limit. Confirms that both paths now go through
`match_against_opposite` — numbers should match `limit_sweep_levels` within
noise (they do).

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
the ~77 ns/op aggregate is a reasonable upper bound for what the engine can
sustain at this size.

## Notes on noise and methodology

- **Setup is not timed.** `iter_batched(setup, body, BatchSize::SmallInput)`
  runs `setup` outside the measured region.
- **Slab capacity is 1 MiB slots** (`SLAB_CAP = 1 << 20`). That makes
  `order_index` start out HashMap-pre-sized for 1M entries, which inflates
  insert-path numbers (`limit_rest`) relative to a smaller-book setup. If you
  ever benchmark pure insert throughput at production sizes, drop `SLAB_CAP`.
- **`limit_sweep` ≈ `market_sweep`** at every level count: both paths now
  delegate to `match_against_opposite`, so the only difference is the
  per-iteration limit-price check (~1 cycle, masked by HashMap latency).
- All numbers above are from a single host. Re-run on the actual deployment
  hardware before drawing conclusions about absolute throughput.
