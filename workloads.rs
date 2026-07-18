//! Variant-agnostic workload primitives — single source of truth for the op
//! streams consumed by benches, the profiler, and tests.
//!
//! Deliberately lives outside `src/`: this is bench/test infrastructure, not
//! part of the production library. Consumers (`tests/*.rs`, `benches/*.rs`,
//! `profiling/*.rs`) pull it in with a `#[path = "../workloads.rs"] mod
//! workloads;` line at the top. Cargo never sees it during a library build.
//!
//! A workload is a `Vec<Op>` (pure data, deterministic from its seed) plus a
//! `Harness<B>` that applies the stream to any `B: OrderBookApi`. Ops use
//! caller-defined `u64` logical ids; the harness maintains the
//! `logical_id → engine handle` map so cancels can reference orders by id
//! even though the engine mints the handles itself.

use std::collections::{HashMap, VecDeque};

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use calvera_books::api::OrderBookApi;
use calvera_books::errors::BookResult;
use calvera_books::types::{MarketOrderMode, Price, Side, SlabAllocator};

#[path = "scenarios.rs"]
mod scenarios;
use scenarios::{Event, Scenario};

/// A single market-affecting action. `id`s are workload-local.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    Limit { id: u64, side: Side, price: Price, qty: u64 },
    Cancel { id: u64 },
    /// Market orders never rest. Currently unused by the warm `mixed`
    /// workload (it's add+cancel only); M6 scenarios will emit these.
    #[allow(dead_code)]
    Market { side: Side, qty: u64, mode: MarketOrderMode },
}

/// Drives an op stream against any `B: OrderBookApi`, mapping logical ids to
/// engine-assigned handles. `apply` is `#[inline]` so the criterion hot loop
/// monomorphises through it.
pub struct Harness<B: OrderBookApi> {
    pub book: B,
    pub handles: HashMap<u64, B::Handle>,
}

impl<B: OrderBookApi> Harness<B> {
    /// Convenience for the default System allocator. Used by tests; the bench
    /// runner and profiler go through `new_with_alloc` to thread an explicit
    /// `SlabAllocator` choice.
    #[allow(dead_code)]
    pub fn new(slab_capacity: usize) -> Self {
        Self { book: B::new(slab_capacity), handles: HashMap::new() }
    }

    /// Build a harness whose book is constructed with the given allocator.
    /// Returns `Err(UnsupportedAllocator)` when the platform doesn't support
    /// the requested variant.
    pub fn new_with_alloc(slab_capacity: usize, alloc: SlabAllocator) -> BookResult<Self> {
        Ok(Self {
            book: B::new_with_alloc(slab_capacity, alloc)?,
            handles: HashMap::new(),
        })
    }

    #[inline]
    pub fn apply(&mut self, op: &Op) {
        match *op {
            Op::Limit { id, side, price, qty } => {
                if let Ok(Some(h)) = self.book.add_limit(side, price, qty) {
                    self.handles.insert(id, h);
                }
            }
            Op::Cancel { id } => {
                if let Some(h) = self.handles.remove(&id) {
                    let _ = self.book.cancel(h);
                }
            }
            Op::Market { side, qty, mode } => {
                let _ = self.book.add_market(side, qty, mode);
            }
        }
    }

    #[inline]
    pub fn apply_all(&mut self, ops: &[Op]) {
        for op in ops {
            self.apply(op);
        }
    }
}

// ---------------------------------------------------------------------------
// Op-stream generators
// ---------------------------------------------------------------------------

/// Pre-populate a book with `levels` price levels per side, `orders_per_level`
/// orders at each, around `mid`. Bids below mid, asks above. Logical ids run
/// `start_id..start_id + levels*orders_per_level*2`.
pub fn populate_uniform_ops(
    levels: u64,
    orders_per_level: u64,
    mid: u64,
    start_id: u64,
) -> Vec<Op> {
    let n = (levels * orders_per_level * 2) as usize;
    let mut ops = Vec::with_capacity(n);
    let mut id = start_id;
    for i in 1..=levels {
        for _ in 0..orders_per_level {
            ops.push(Op::Limit { id, side: Side::Bid, price: Price(mid - i), qty: 1 });
            id += 1;
        }
        for _ in 0..orders_per_level {
            ops.push(Op::Limit { id, side: Side::Ask, price: Price(mid + i), qty: 1 });
            id += 1;
        }
    }
    ops
}

/// Deterministic mixed add/cancel stream — same shape as the existing
/// `mixed_workload/random_add_cancel` bench:
/// - `add_ratio` of ops are adds (random side, qty 1, price = `mid` ± offset
///   in `1..=spread_ticks`), the rest are cancels of an own-issued id.
/// - Ids start at `start_id + 1` and increase monotonically.
///
/// `StdRng` (ChaCha-based) is used for byte-stability across rebuilds and
/// architectures; the same `seed` always produces the same stream.
pub fn mixed_workload_ops(
    seed: u64,
    n_ops: usize,
    mid: u64,
    spread_ticks: u64,
    add_ratio: f64,
    start_id: u64,
) -> Vec<Op> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut ops = Vec::with_capacity(n_ops);
    let mut next_id = start_id;
    for _ in 0..n_ops {
        // Force an add if nothing has been issued yet (cancel range would be empty).
        let do_add = next_id == start_id || rng.random_bool(add_ratio);
        if do_add {
            next_id += 1;
            let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
            let offset = rng.random_range(1..=spread_ticks);
            let price = match side {
                Side::Bid => Price(mid - offset),
                Side::Ask => Price(mid + offset),
            };
            ops.push(Op::Limit { id: next_id, side, price, qty: 1 });
        } else {
            let target = rng.random_range((start_id + 1)..=next_id);
            ops.push(Op::Cancel { id: target });
        }
    }
    ops
}

// ---------------------------------------------------------------------------
// Warm-state workload abstraction (M3)
// ---------------------------------------------------------------------------

/// A warm-state workload: one `setup` that builds per-iter state ahead of
/// time, plus a `hot` function the runner calls in a tight loop. Whatever
/// allocation/first-touch/HashMap-bucket cost the engine has lands inside
/// `setup`, not the timed body.
///
/// `S` is the workload-specific state — typically a `Harness<B>` plus a
/// precomputed `Vec<Op>` and a cursor. Each workload picks its own `S`.
pub struct Workload<S> {
    pub name: &'static str,
    pub slab_cap: usize,
    /// Iterations of `hot` run untimed before the timed loop. Provides "soft"
    /// prefault by exercising real engine work — touches slab pages and warms
    /// HashMap buckets that subsequent timed iters will hit. Explicit byte-
    /// per-page prefault is deferred until M3.5 validation shows it's needed.
    pub warmup_iters: usize,
    /// Build per-iter state. Fallible because non-default `SlabAllocator`
    /// variants (`MadvHugepage`, `Hugetlb`) are Linux-only and may also fail
    /// at runtime (e.g. empty hugepage pool).
    pub setup: fn(usize, SlabAllocator) -> BookResult<S>,
    pub hot: fn(&mut S),
}

/// Per-iter state for the mixed add/cancel workload.
pub struct MixedState<B: OrderBookApi> {
    pub harness: Harness<B>,
    pub ops: Vec<Op>,
    pub idx: usize,
}

/// Build a steady-state mixed workload: populate the book with 50 levels × 4
/// orders/side (= 400 resting orders), then precompute 1024 random add/cancel
/// ops at a 50/50 ratio so the book size stays bounded under arbitrarily long
/// runs. Cancels target own-issued ids only.
pub fn setup_mixed_warm<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<MixedState<B>> {
    let mut harness = Harness::<B>::new_with_alloc(slab_cap, alloc)?;
    let populate = populate_uniform_ops(50, 4, 10_000, 1);
    harness.apply_all(&populate);
    // 50/50 add:cancel — net-zero growth in own-issued orders.
    let ops = mixed_workload_ops(0xC0FFEE, 1024, 10_000, 50, 0.5, 100_000);
    Ok(MixedState { harness, ops, idx: 0 })
}

/// One op per call; cursor wraps around the precomputed ops vec so the
/// runner can call this indefinitely.
#[inline]
pub fn hot_mixed<B: OrderBookApi>(state: &mut MixedState<B>) {
    let op = &state.ops[state.idx];
    state.harness.apply(op);
    state.idx = state.idx.wrapping_add(1);
    if state.idx == state.ops.len() {
        state.idx = 0;
    }
}

// ---------------------------------------------------------------------------
// `add_cancel` — tightest possible alloc/free loop. Hot alternates
// add(side=Bid, price=100, qty=1) and cancel(just-added). Book holds 0 or 1
// orders at any moment; per-op cost averages (add + cancel) / 2. Useful as a
// floor for the allocation/index path.
// ---------------------------------------------------------------------------

pub struct AddCancelState<B: OrderBookApi> {
    pub book: B,
    pub resting: Option<B::Handle>,
}

pub fn setup_add_cancel<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<AddCancelState<B>> {
    Ok(AddCancelState { book: B::new_with_alloc(slab_cap, alloc)?, resting: None })
}

#[inline]
pub fn hot_add_cancel<B: OrderBookApi>(state: &mut AddCancelState<B>) {
    match state.resting {
        None => {
            if let Ok(Some(h)) = state.book.add_limit(Side::Bid, Price(100), 1) {
                state.resting = Some(h);
            }
        }
        Some(h) => {
            let _ = state.book.cancel(h);
            state.resting = None;
        }
    }
}

pub fn add_cancel_workload<B: OrderBookApi>() -> Workload<AddCancelState<B>> {
    Workload {
        name: "add_cancel",
        // Small slab — one slot in use at a time, no point pre-sizing for 1M.
        slab_cap: 1 << 10,
        warmup_iters: 1_000,
        setup: setup_add_cancel::<B>,
        hot: hot_add_cancel::<B>,
    }
}

// ---------------------------------------------------------------------------
// `add_spread` — stresses BTreeSet price-index + new-level allocation.
//
// FIFO of `SPREAD_TARGET` outstanding orders cycling through `SPREAD_K`
// distinct prices (K=256, target=K/2). Each iter adds at the next price in
// the cycle and cancels the oldest. Because target = K/2, the FIFO's oldest
// entry is always at a price K/2 ahead of the new add's price; so each iter
// the cancel drains a level (sole order removed → BTreeSet remove) AND the
// add creates a level (price was empty since K/2 iters ago → BTreeSet
// insert).
// ---------------------------------------------------------------------------

const SPREAD_K: u64 = 256;
const SPREAD_TARGET: usize = 128; // = SPREAD_K / 2
const SPREAD_MID: u64 = 10_000;

pub struct AddSpreadState<B: OrderBookApi> {
    pub book: B,
    pub handles: VecDeque<B::Handle>,
    pub price_cursor: u64,
}

pub fn setup_add_spread<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<AddSpreadState<B>> {
    let mut s = AddSpreadState {
        book: B::new_with_alloc(slab_cap, alloc)?,
        handles: VecDeque::with_capacity(SPREAD_TARGET + 1),
        price_cursor: 0,
    };
    // Fill the FIFO to its steady-state depth. After this, every hot iter is
    // exactly one create + one drain.
    for _ in 0..SPREAD_TARGET {
        let price = SPREAD_MID + (s.price_cursor % SPREAD_K);
        s.price_cursor += 1;
        if let Ok(Some(h)) = s.book.add_limit(Side::Bid, Price(price), 1) {
            s.handles.push_back(h);
        }
    }
    Ok(s)
}

#[inline]
pub fn hot_add_spread<B: OrderBookApi>(s: &mut AddSpreadState<B>) {
    let price = SPREAD_MID + (s.price_cursor % SPREAD_K);
    s.price_cursor = s.price_cursor.wrapping_add(1);
    if let Ok(Some(h)) = s.book.add_limit(Side::Bid, Price(price), 1) {
        s.handles.push_back(h);
    }
    if let Some(old) = s.handles.pop_front() {
        let _ = s.book.cancel(old);
    }
}

pub fn add_spread_workload<B: OrderBookApi>() -> Workload<AddSpreadState<B>> {
    Workload {
        name: "add_spread",
        // Need to hold SPREAD_TARGET + 1 in the slab briefly; 512 is plenty.
        slab_cap: 1 << 10,
        warmup_iters: 1_000,
        setup: setup_add_spread::<B>,
        hot: hot_add_spread::<B>,
    }
}

// ---------------------------------------------------------------------------
// `cancel_heavy` — stresses the per-level FIFO ops at a single price.
//
// Pre-populates `HEAVY_DEPTH` orders at one price, then each hot iter does
// add-at-tail + cancel-head of that level. No level create/drain (the level
// never empties); pure linked-list head-update + tail-append work plus the
// engine's index/handle bookkeeping.
// ---------------------------------------------------------------------------

const HEAVY_DEPTH: usize = 50;
const HEAVY_PRICE: u64 = 100;

pub struct CancelHeavyState<B: OrderBookApi> {
    pub book: B,
    pub handles: VecDeque<B::Handle>,
}

pub fn setup_cancel_heavy<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<CancelHeavyState<B>> {
    let mut s = CancelHeavyState {
        book: B::new_with_alloc(slab_cap, alloc)?,
        handles: VecDeque::with_capacity(HEAVY_DEPTH + 1),
    };
    for _ in 0..HEAVY_DEPTH {
        if let Ok(Some(h)) = s.book.add_limit(Side::Bid, Price(HEAVY_PRICE), 1) {
            s.handles.push_back(h);
        }
    }
    Ok(s)
}

#[inline]
pub fn hot_cancel_heavy<B: OrderBookApi>(s: &mut CancelHeavyState<B>) {
    if let Ok(Some(h)) = s.book.add_limit(Side::Bid, Price(HEAVY_PRICE), 1) {
        s.handles.push_back(h);
    }
    if let Some(old) = s.handles.pop_front() {
        let _ = s.book.cancel(old);
    }
}

pub fn cancel_heavy_workload<B: OrderBookApi>() -> Workload<CancelHeavyState<B>> {
    Workload {
        name: "cancel_heavy",
        slab_cap: 1 << 10,
        warmup_iters: 1_000,
        setup: setup_cancel_heavy::<B>,
        hot: hot_cancel_heavy::<B>,
    }
}

// ---------------------------------------------------------------------------
// `match_single` — isolates the full-consume match path at a single level.
//
// Pre-populates `MATCH_DEPTH` asks at one price, then each hot iter adds one
// ask at the tail and sends a crossing bid that fully consumes the head ask
// (qty 1 vs qty 1). Because we add *before* we consume, the level depth
// oscillates MATCH_DEPTH → MATCH_DEPTH+1 → MATCH_DEPTH and never reaches zero,
// so the level is never created/drained — no BTreeSet churn. What's measured
// is the aggressor's `match_against_opposite` full-consume branch (freed-chain
// stitch, generation bump, slab reclaim) plus the resting add. The consuming
// bid fully fills and rests nothing, so no handle bookkeeping is needed;
// matching walks the level's FIFO by itself.
// ---------------------------------------------------------------------------

const MATCH_DEPTH: usize = 16;
const MATCH_PRICE: u64 = 100;

pub struct MatchSingleState<B: OrderBookApi> {
    pub book: B,
}

pub fn setup_match_single<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<MatchSingleState<B>> {
    let mut book = B::new_with_alloc(slab_cap, alloc)?;
    for _ in 0..MATCH_DEPTH {
        let _ = book.add_limit(Side::Ask, Price(MATCH_PRICE), 1);
    }
    Ok(MatchSingleState { book })
}

#[inline]
pub fn hot_match_single<B: OrderBookApi>(s: &mut MatchSingleState<B>) {
    // Replenish the tail, then cross the head. Net depth is constant.
    let _ = s.book.add_limit(Side::Ask, Price(MATCH_PRICE), 1);
    let _ = s.book.add_limit(Side::Bid, Price(MATCH_PRICE), 1);
}

pub fn match_single_workload<B: OrderBookApi>() -> Workload<MatchSingleState<B>> {
    Workload {
        name: "match_single",
        // Holds MATCH_DEPTH + 1 briefly; 1K is plenty.
        slab_cap: 1 << 10,
        warmup_iters: 1_000,
        setup: setup_match_single::<B>,
        hot: hot_match_single::<B>,
    }
}

// ---------------------------------------------------------------------------
// `sweep` — multi-level market sweep. Stresses the destructive sweep across
// many price levels: per hot iter one market order drains `SWEEP_LEVELS`
// distinct single-order levels (full consume + level removal + BTreeSet remove
// + best-price refresh, once per level).
//
// Matching is destructive, so steady state uses a multi-strip pre-population:
// setup rests `SWEEP_STRIPS * SWEEP_LEVELS` asks, one per distinct price. Each
// hot iter's market bid (qty = SWEEP_LEVELS, 1 unit per level) consumes the
// `SWEEP_LEVELS` lowest levels; after `SWEEP_STRIPS` iters the book is empty
// and the next iter refills all strips. The refill is a burst amortised over
// `SWEEP_STRIPS` iters (~1/64 of iters here), so the timed mean is dominated by
// the sweep itself, not the rebuild. No handles: the market order consumes by
// price, and the refill's resting handles are irrelevant (all are swept out).
// ---------------------------------------------------------------------------

const SWEEP_LEVELS: u64 = 8; // price levels drained per hot iter
const SWEEP_STRIPS: usize = 64; // iters between refills
const SWEEP_MID: u64 = 10_000;

pub struct SweepState<B: OrderBookApi> {
    pub book: B,
    /// Strips of liquidity remaining before the book empties and needs a
    /// refill. Starts at `SWEEP_STRIPS`; decremented once per hot iter.
    pub strips_left: usize,
}

/// (Re)populate the ask side with `SWEEP_STRIPS * SWEEP_LEVELS` single-order
/// levels at contiguous prices above `SWEEP_MID`. Called from setup and from
/// the hot loop's refill branch.
fn populate_sweep_asks<B: OrderBookApi>(book: &mut B) {
    let total = SWEEP_STRIPS as u64 * SWEEP_LEVELS;
    for i in 1..=total {
        let _ = book.add_limit(Side::Ask, Price(SWEEP_MID + i), 1);
    }
}

pub fn setup_sweep<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<SweepState<B>> {
    let mut book = B::new_with_alloc(slab_cap, alloc)?;
    populate_sweep_asks(&mut book);
    Ok(SweepState {
        book,
        strips_left: SWEEP_STRIPS,
    })
}

#[inline]
pub fn hot_sweep<B: OrderBookApi>(s: &mut SweepState<B>) {
    if s.strips_left == 0 {
        populate_sweep_asks(&mut s.book);
        s.strips_left = SWEEP_STRIPS;
    }
    // One market order sweeps the SWEEP_LEVELS lowest asks (1 unit each).
    let _ = s
        .book
        .add_market(Side::Bid, SWEEP_LEVELS, MarketOrderMode::ImmediateOrCancel);
    s.strips_left -= 1;
}

pub fn sweep_workload<B: OrderBookApi>() -> Workload<SweepState<B>> {
    Workload {
        name: "sweep",
        // Holds SWEEP_STRIPS * SWEEP_LEVELS (= 512) orders between refills.
        slab_cap: 1 << 10,
        warmup_iters: 1_000,
        setup: setup_sweep::<B>,
        hot: hot_sweep::<B>,
    }
}

// ---------------------------------------------------------------------------
// `deep_book` — large book with a realistic power-law (Pareto-like) depth
// profile: liquidity is densest at the mid and thins with distance. Built to
// stress the per-side slab: deep same-side chains whose working set
// exceeds L1.
//
// Shape: per side, `DEEP_LEVELS` price levels; level `d` (d ticks off mid)
// holds `orders(d) = max(1, round(PEAK / d^ALPHA))` orders of qty 1. Liquidity
// is modelled as order *count* per level (the matcher-relevant dimension —
// what the FIFO walk traverses and the slab holds), so "80% of liquidity near
// the price" means 80% of a side's resting orders sit in the near band. Both
// sides are populated with bid/ask allocations interleaved, so a shared slab
// lays same-side orders across mixed cache lines; a per-side slab packs them.
//
// Steady-state hot iter: one market order consumes exactly the near-mid band
// (`DEEP_NEAR_LEVELS` levels — a long same-side chain walk), then the band is
// rebuilt, restoring the book to its initial deep shape. The deep tail rests
// permanently (footprint / TLB pressure); the opposite side rests as the
// interleaving "pollution" a shared slab pays for. Deterministic, leak-free.
// ---------------------------------------------------------------------------

const DEEP_MID: u64 = 1_000_000;
const DEEP_LEVELS: u64 = 256; // price levels per side
const DEEP_NEAR_LEVELS: u64 = 80; // churned near-mid band (holds ~80% of orders)
const DEEP_PEAK: f64 = 1000.0; // orders at the innermost level
const DEEP_ALPHA: f64 = 1.0; // power-law decay exponent

/// Orders resting at level `d` (d ticks off mid): a floored power law.
#[inline]
fn deep_orders_at(d: u64) -> u64 {
    let raw = DEEP_PEAK / (d as f64).powf(DEEP_ALPHA);
    (raw.round() as u64).max(1)
}

/// Total orders in the near-mid band (one side) — also the market-order qty
/// that consumes exactly that band.
fn deep_near_band_qty() -> u64 {
    (1..=DEEP_NEAR_LEVELS).map(deep_orders_at).sum()
}

/// Populate both sides with the power-law depth profile, interleaving bid/ask
/// allocations so a shared slab mixes sides across adjacent slots.
fn populate_deep_book<B: OrderBookApi>(book: &mut B) {
    for d in 1..=DEEP_LEVELS {
        for _ in 0..deep_orders_at(d) {
            let _ = book.add_limit(Side::Bid, Price(DEEP_MID - d), 1);
            let _ = book.add_limit(Side::Ask, Price(DEEP_MID + d), 1);
        }
    }
}

pub struct DeepBookState<B: OrderBookApi> {
    pub book: B,
    /// Cached qty that consumes exactly the near-mid ask band.
    pub near_qty: u64,
}

pub fn setup_deep_book<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<DeepBookState<B>> {
    let mut book = B::new_with_alloc(slab_cap, alloc)?;
    populate_deep_book(&mut book);
    Ok(DeepBookState {
        book,
        near_qty: deep_near_band_qty(),
    })
}

#[inline]
pub fn hot_deep_book<B: OrderBookApi>(s: &mut DeepBookState<B>) {
    // Sweep the near-mid band: a market bid whose qty equals the band's order
    // count consumes exactly levels 1..=DEEP_NEAR_LEVELS (best-first), walking
    // a long same-side ask chain.
    let _ = s
        .book
        .add_market(Side::Bid, s.near_qty, MarketOrderMode::ImmediateOrCancel);
    // Rebuild the band, restoring the deep shape (the tail below rests).
    for d in 1..=DEEP_NEAR_LEVELS {
        for _ in 0..deep_orders_at(d) {
            let _ = s.book.add_limit(Side::Ask, Price(DEEP_MID + d), 1);
        }
    }
}

pub fn deep_book_workload<B: OrderBookApi>() -> Workload<DeepBookState<B>> {
    Workload {
        name: "deep_book",
        // Holds the full two-sided deep book (~12 K orders) with headroom for
        // the near-band rebuild transient.
        slab_cap: 1 << 15,
        // Each iter is heavy (sweep + rebuild ~10 K ops), so a few warm passes
        // suffice to prefault and reach steady state.
        warmup_iters: 20,
        setup: setup_deep_book::<B>,
        hot: hot_deep_book::<B>,
    }
}

// ---------------------------------------------------------------------------
// Scenario-backed workloads (M6). Each wraps a named scenario from
// `scenarios.rs` (OU-driven mid + Student-t jumps + market-maker cancel/replace
// rhythm). The scenario emits a pure `Vec<Event>`; setup converts it to `Op`
// once and `hot` replays one op per call, wrapping around the precomputed vec.
//
// Scenarios are cycle-clean (their tail drains every live quote), so wrapping
// re-enters a consistent state — no slab leak across long bench runs. Ids are
// reused each cycle, so the harness `id → handle` map stays bounded.
// ---------------------------------------------------------------------------

/// Events per scenario stream. One wrap of this is the bench's replay cycle.
const SCENARIO_N_EVENTS: usize = 4096;

/// Translate a scenario `Event` into the harness's internal `Op`.
#[inline]
fn event_to_op(e: Event) -> Op {
    match e {
        Event::LimitAdd {
            id,
            side,
            price,
            qty,
        } => Op::Limit {
            id,
            side,
            price: Price(price),
            qty,
        },
        Event::Cancel { id } => Op::Cancel { id },
        Event::Market { side, qty, mode } => Op::Market { side, qty, mode },
    }
}

/// Per-iter state shared by every scenario workload: a warm harness plus the
/// precomputed op stream and a wrap-around cursor.
pub struct ScenarioState<B: OrderBookApi> {
    pub harness: Harness<B>,
    pub ops: Vec<Op>,
    pub idx: usize,
}

/// Build scenario state: precompute the event stream (untimed) and convert to
/// ops. Generic over the scenario so each named workload is a one-line setup.
fn setup_scenario<B: OrderBookApi>(
    scenario: &dyn Scenario,
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<ScenarioState<B>> {
    let harness = Harness::<B>::new_with_alloc(slab_cap, alloc)?;
    let ops = scenario
        .generate(SCENARIO_N_EVENTS)
        .into_iter()
        .map(event_to_op)
        .collect();
    Ok(ScenarioState {
        harness,
        ops,
        idx: 0,
    })
}

#[inline]
pub fn hot_scenario<B: OrderBookApi>(state: &mut ScenarioState<B>) {
    let op = &state.ops[state.idx];
    state.harness.apply(op);
    state.idx = state.idx.wrapping_add(1);
    if state.idx == state.ops.len() {
        state.idx = 0;
    }
}

/// Emit the workload constructor + its (capture-free) setup fn for a named
/// scenario. `Workload::setup` is a bare `fn` pointer, so each scenario needs
/// its own monomorphic setup that names its generator.
macro_rules! scenario_workload {
    ($ctor:ident, $setup:ident, $scenario:path, $name:literal) => {
        fn $setup<B: OrderBookApi>(
            slab_cap: usize,
            alloc: SlabAllocator,
        ) -> BookResult<ScenarioState<B>> {
            setup_scenario::<B>(&$scenario(), slab_cap, alloc)
        }

        pub fn $ctor<B: OrderBookApi>() -> Workload<ScenarioState<B>> {
            Workload {
                name: $name,
                slab_cap: 1 << 14,
                // One full pass of the stream as warmup, so the timed body
                // starts in steady state (and the stream drains once cleanly).
                warmup_iters: SCENARIO_N_EVENTS,
                setup: $setup::<B>,
                hot: hot_scenario::<B>,
            }
        }
    };
}

scenario_workload!(
    calm_market_workload,
    setup_calm_market,
    scenarios::calm_market,
    "calm_market"
);
scenario_workload!(
    news_event_workload,
    setup_news_event,
    scenarios::news_event,
    "news_event"
);
scenario_workload!(
    illiquid_workload,
    setup_illiquid,
    scenarios::illiquid,
    "illiquid"
);
scenario_workload!(
    opening_auction_workload,
    setup_opening_auction,
    scenarios::opening_auction,
    "opening_auction"
);

/// Constructor for the mixed workload, parameterised over the variant.
pub fn mixed_workload<B: OrderBookApi>() -> Workload<MixedState<B>> {
    Workload {
        name: "mixed",
        // 16K slots — per `lessons/pages.md`, this size is appropriate for
        // workloads that don't actually need 1M; keeps first-touch cost down.
        slab_cap: 1 << 14,
        warmup_iters: 1_000,
        setup: setup_mixed_warm::<B>,
        hot: hot_mixed::<B>,
    }
}

/// Stand-in runner — runs setup, warmup, then a timed loop of `iters` hot
/// calls. Returns the elapsed time of the timed loop only. Used by the
/// workload smoke tests; the real criterion runner is `benches/engine.rs`.
#[allow(dead_code)]
pub fn run_warm<S>(w: &Workload<S>, iters: u64) -> std::time::Duration {
    let mut state = (w.setup)(w.slab_cap, SlabAllocator::System)
        .expect("SlabAllocator::System never fails");
    for _ in 0..w.warmup_iters {
        (w.hot)(&mut state);
    }
    let start = std::time::Instant::now();
    for _ in 0..iters {
        (w.hot)(&mut state);
    }
    start.elapsed()
}

#[cfg(test)]
// `cargo bench` also enables `cfg(test)`, so this mod gets compiled into the
// bench binary even though only the criterion harness runs there. Silence
// the resulting "unused" warnings for items that are exercised by
// `cargo test` but not by the bench.
#[allow(dead_code, unused_imports)]
mod tests {
    use super::*;
    use calvera_books::orderbook::{OrderBook, VecConsumer};

    type Book = OrderBook<VecConsumer>;

    fn drive<B: OrderBookApi>(populate: &[Op], mixed: &[Op]) -> usize {
        let mut h = Harness::<B>::new(1 << 14);
        h.apply_all(populate);
        h.apply_all(mixed);
        h.handles.len()
    }

    #[test]
    fn populate_then_mixed_drives_the_book() {
        let populate = populate_uniform_ops(20, 2, 10_000, 1);
        let mixed = mixed_workload_ops(0xC0FFEE, 1024, 10_000, 50, 0.7, 100_000);
        let n = drive::<Book>(&populate, &mixed);
        assert!(n > 0);
    }

    #[test]
    fn warm_mixed_runs_steady_state() {
        // 100k op iters — exceeds the slab cap many times over;
        // would `SlabFull` if the workload weren't steady-state.
        let d = run_warm(&mixed_workload::<Book>(), 100_000);
        assert!(d > std::time::Duration::ZERO);
    }

    #[test]
    fn warm_matching_workloads_survive() {
        // Destructive-matching workloads: many iters must not exhaust the slab
        // (match_single is depth-bounded; sweep refills every SWEEP_STRIPS).
        // 500k iters — a SlabFull or bad steady-state would panic.
        for iters in [500_000u64] {
            let _ = run_warm(&match_single_workload::<Book>(), iters);
            let _ = run_warm(&sweep_workload::<Book>(), iters);
        }
    }

    #[test]
    fn deep_book_is_concentrated_and_survives() {
        // Liquidity concentration: the near band should hold the bulk of a
        // side's orders (the "~80% near the price" shape).
        let total: u64 = (1..=DEEP_LEVELS).map(deep_orders_at).sum();
        let near = deep_near_band_qty();
        let pct = near as f64 / total as f64 * 100.0;
        eprintln!(
            "deep_book: {} orders/side across {} levels; near band ({} levels) = {} orders ({:.0}%); \
             two-sided footprint ~{} KiB",
            total,
            DEEP_LEVELS,
            DEEP_NEAR_LEVELS,
            near,
            pct,
            (total * 2 * 32) / 1024,
        );
        assert!(
            pct >= 75.0,
            "near band should hold ~80% of a side's liquidity, got {pct:.0}%"
        );
        // Survival (each iter is heavy; 200 iters is plenty to prove
        // steady state — a leak or SlabFull would panic).
        let _ = run_warm(&deep_book_workload::<Book>(), 200);
    }

    #[test]
    fn scenarios_are_deterministic() {
        // A fixed (params, seed) must yield a byte-identical stream every call
        // — the property that lets a scenario be shared across hosts/runs.
        for s in scenarios::all() {
            let a = s.generate(2048);
            let b = s.generate(2048);
            assert_eq!(a.len(), b.len(), "scenario {} length drift", s.name());
            assert!(a == b, "scenario {} is not deterministic", s.name());
        }
    }

    #[test]
    fn scenario_workloads_run() {
        // Each scenario replayed 50k iters (>> stream length, so it wraps).
        // A leak or bad steady state would SlabFull.
        let _ = run_warm(&calm_market_workload::<Book>(), 50_000);
        let _ = run_warm(&news_event_workload::<Book>(), 50_000);
        let _ = run_warm(&illiquid_workload::<Book>(), 50_000);
        let _ = run_warm(&opening_auction_workload::<Book>(), 50_000);
    }

    #[test]
    fn mixed_workload_is_deterministic() {
        let a = mixed_workload_ops(0xCAFEBABE, 256, 10_000, 50, 0.7, 100_000);
        let b = mixed_workload_ops(0xCAFEBABE, 256, 10_000, 50, 0.7, 100_000);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            // Op is Copy; compare via formatted form since it's not Eq.
            assert_eq!(format!("{x:?}"), format!("{y:?}"));
        }
    }
}
