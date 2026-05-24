# calvera-books — Changelog

## v0.0.5 — Specialized `u64` hasher

**Change.** Replaced the stdlib HashMap's default SipHash-1-3 with a SplitMix64
finalizer (`U64Mixer`) used for both `order_index: U64Map<OrderId, …>` and
`HalfBook.levels: U64Map<Price, …>`. Both keys are `#[derive(Hash)]` newtypes
around a single `u64`, so the hasher only ever sees one `write_u64` per key —
no `write` path, no length mixing, no per-byte loop. Three multiplies and three
xor-shifts, fully inlined.

A tripwire `write(&mut self, _: &[u8])` `unreachable!()` keeps the assumption
honest: if a future key sprouts a second field, derive switches to per-field
`write_*` calls and this panics rather than silently degrading.

**Why.** Profiling showed `hashbrown::raw::RawTable` operations dominating the
hot path, and HashDoS resistance is irrelevant in this process — every key is
an internally-issued integer. SipHash is two orders of magnitude more work
than necessary for one-`u64` keys.

**Perf.** Headline reductions vs v0.0.4 (median, same harness):

| Benchmark                          | v0.0.4       | v0.0.5   | Δ        |
| ---------------------------------- | -----------: | -------: | -------: |
| `limit_rest/single_level`          | 2.45 µs      | 101 ns   | **−96%** |
| `limit_rest/spread_levels`         | 2.53 µs      | 106 ns   | **−96%** |
| `limit_match_single/full_consume`  | 137 ns       | 82.5 ns  | −40%     |
| `limit_sweep_levels/4`             | 357 ns       | 133 ns   | −60%     |
| `limit_sweep_levels/256`           | 17.1 µs      | 9.31 µs  | −45%     |
| `market_sweep/256`                 | 16.3 µs      | 9.59 µs  | −41%     |
| `market_sweep_opl/L4xO64`          | 4.44 µs      | 1.79 µs  | −60%     |
| `cancel/mid_book`                  | 80 ns        | 42 ns    | −47%     |
| `mixed_workload/random_add_cancel` | 27.5 µs      | 9.72 µs  | **−65%** |

The `limit_rest` collapse (~24×) is amplified by `SLAB_CAP = 1 << 20`: the
order-index map is pre-sized for 1M entries, so every insert pays the full
SipHash setup cost on an otherwise no-op iteration. With `U64Mixer` that cost
effectively disappears and what's left is alloc + intrusive-list patching.

---

## v0.0.4 — `alloc_slot` instead of `insert_order(Order)`

**Change.** `OrderSlab` no longer accepts a fully-built `Order` by value.
Callers ask for a slot index (`alloc_slot() -> SlabIndex`) and write fields
through `get_mut`. The slab's backing buffer is already 32-aligned, so the
caller never constructs a 32-aligned `Order` on the stack.

**Why.** The `#[repr(align(32))]` from v0.0.3 was forcing the compiler to
align the *stack frame* to 32 in every function that built an `Order` literal,
adding `sub rsp / and rsp, -32` to `add_limit_order`'s prologue. The two extra
instructions were ~0.7 ns directly, but they also cascaded: stack frame grew
from 192 → 256 B, register allocation got worse, instruction scheduling
through the rest of the function degraded.

**Perf.** Wins concentrate at shallow sweeps (`*sweep_*/4` roughly halved) and
~10–25% on every shape that goes through `add_limit_order`.

## v0.0.3 — `NonZeroU32` slab indices + `Order` cache-line alignment

**Change.**
- `SlabIndex(u32)` → `SlabIndex(NonZeroU32)`. `Option<SlabIndex>` niches into
  4 B (free `None` at bit-pattern 0) instead of 8 B.
- Slab reserves slot 0 to honour the non-zero invariant.
- `Order` becomes exactly 32 B (8 + 8 + 8 + 4 + 4) with `#[repr(C, align(32))]`.

**Why.** Two 32-B `Order`s fit cleanly in a 64-B cache line with no straddle
regardless of allocator behaviour. Halving the `Option<SlabIndex>` footprint
also shrinks the intrusive linked-list pointers inside each level walk.

**Perf.** Double-digit reductions across everything with a non-trivial cache
footprint: ~9–17% on deep sweeps, ~22% on cancel, ~29% on the mixed workload.

## v0.0.2 — Per-level batched sweep + intrusive freelist + cancel slot-leak fix

**Change.**
- `match_against_opposite` restructured into outer sweep (price levels) + inner
  level walk (orders within a level). The `PriceLevel` is cached once per
  level instead of once per fill.
- Fully-consumed orders are gathered via the existing FIFO `Order.next` chain
  into a per-level local chain, stitched across level boundaries with one slab
  write at the boundary, and spliced into `free_head` exactly once at
  end-of-sweep. The per-fill `slab.free` call is gone.
- Dropped the `OrderSlot` enum tag. The slab is now `Vec<Order>` with the
  freelist threaded through `Order.next` of unoccupied slots.
- Fixed: `cancel_limit_order` was never freeing the slab slot, leaking one
  slot per cancel. The post-fix `cancel/mid_book` (127 ns) is higher than the
  pre-fix number (91 ns) — pre-fix was simply doing less work.

**Why.** The old per-fill structure paid HashMap-of-levels lookup + slab-free
overhead on every fill. Most fills inside a sweep target the same level, so
those costs amortize to ~zero if you walk the level head once.

**Perf.** The big win lands on OPL-batched sweeps where many fills share a
level — `market_sweep_opl` improvements of 40–55% on the `L64xO4 / L16xO16 /
L4xO64` shapes (see `BENCHMARKS.md` for the full table).

## v0.0.1 — Initial implementation

`OrderBook` keyed by `HashMap<Price, PriceLevel>` per side, with a global
`HashMap<OrderId, …>` for O(1) cancel lookups. Slab held an `OrderSlot` enum
(`Occupied | Free`). Matching consumed one resting order per loop iteration,
freeing slab slots inline. This is the baseline against which the "v1" column
in [`BENCHMARKS.md`](BENCHMARKS.md) is reported.