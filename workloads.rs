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
// `calm_market` — M6 scenario layer: OU-driven mid + simple market-maker
// cancel/replace rhythm. Emits a pure `Vec<Op>` stream; the workload setup
// precomputes it once and `hot` replays one op per call.
//
// Cycle-clean: every add has a matching cancel within the stream (drain
// step at the end), so when the runner wraps around the precomputed vec the
// harness state is consistent — no slab leaks across long bench runs.
// ---------------------------------------------------------------------------

const CALM_MM_DEPTH: usize = 10;
const CALM_AGGRESSOR_RATE: f64 = 0.01;
const CALM_MID: f64 = 10_000.0;
const CALM_THETA: f64 = 0.05; // OU mean-reversion rate per step
const CALM_SIGMA: f64 = 0.5; // OU innovation std-dev (in ticks)
const CALM_QUOTE_OFFSETS: u64 = 5; // each MM quote sits 1..=5 ticks away from mid
const CALM_N_EVENTS: usize = 4096;
const CALM_SEED: u64 = 0xCA1_DA7A;

/// Standard-normal sample via Box-Muller. Two uniforms in → one normal out.
/// Discards the second-half pair to keep state minimal; for a scenario-scale
/// generator the throughput tradeoff is irrelevant.
fn box_muller(rng: &mut StdRng) -> f64 {
    // Guard against u1 == 0 producing ln(0) = -inf.
    let mut u1: f64 = rng.random_range(0.0..1.0);
    if u1 == 0.0 {
        u1 = f64::MIN_POSITIVE;
    }
    let u2: f64 = rng.random_range(0.0..1.0);
    let r = (-2.0 * u1.ln()).sqrt();
    let theta = 2.0 * std::f64::consts::PI * u2;
    r * theta.cos()
}

/// Generate an OU + market-maker event stream. Calm-market regime: low
/// volatility, no jumps, ~1% market-order rate. Cycle-clean (drains to zero
/// live quotes at the end).
fn calm_market_events(seed: u64, n_events: usize) -> Vec<Op> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut events = Vec::with_capacity(n_events);

    let mut next_id: u64 = 100_000;
    let mut live: VecDeque<u64> = VecDeque::with_capacity(CALM_MM_DEPTH + 1);
    let mut mid: f64 = CALM_MID;

    // Reserve the last `CALM_MM_DEPTH` events for the drain so the stream is
    // self-balancing (every add has a cancel).
    let main_n = n_events.saturating_sub(CALM_MM_DEPTH);

    while events.len() < main_n {
        // OU step. dX = θ(μ − X) dt + σ ε, where ε is standard normal.
        // Discretised with dt = 1 implicit.
        let z = box_muller(&mut rng);
        mid += CALM_THETA * (CALM_MID - mid) + CALM_SIGMA * z;
        let tick = mid.round() as u64;

        // 1% chance: aggressor market order. Doesn't touch the MM deque.
        if rng.random_bool(CALM_AGGRESSOR_RATE) {
            let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
            events.push(Op::Market { side, qty: 1, mode: MarketOrderMode::ImmediateOrCancel });
            continue;
        }

        // MM rhythm: keep deque around CALM_MM_DEPTH.
        // If full, cancel oldest before adding the new quote.
        if live.len() >= CALM_MM_DEPTH {
            if let Some(id) = live.pop_front() {
                events.push(Op::Cancel { id });
                if events.len() >= main_n {
                    break;
                }
            }
        }

        next_id += 1;
        let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
        let offset = rng.random_range(1..=CALM_QUOTE_OFFSETS);
        let price = match side {
            Side::Bid => tick.saturating_sub(offset),
            Side::Ask => tick.saturating_add(offset),
        };
        events.push(Op::Limit { id: next_id, side, price: Price(price), qty: 1 });
        live.push_back(next_id);
    }

    // Drain phase: cancel every remaining live quote so the stream ends with
    // an empty harness (cycle-clean).
    while let Some(id) = live.pop_front() {
        if events.len() >= n_events {
            break;
        }
        events.push(Op::Cancel { id });
    }

    events
}

pub struct CalmMarketState<B: OrderBookApi> {
    pub harness: Harness<B>,
    pub ops: Vec<Op>,
    pub idx: usize,
}

pub fn setup_calm_market<B: OrderBookApi>(
    slab_cap: usize,
    alloc: SlabAllocator,
) -> BookResult<CalmMarketState<B>> {
    let harness = Harness::<B>::new_with_alloc(slab_cap, alloc)?;
    let ops = calm_market_events(CALM_SEED, CALM_N_EVENTS);
    Ok(CalmMarketState { harness, ops, idx: 0 })
}

#[inline]
pub fn hot_calm_market<B: OrderBookApi>(state: &mut CalmMarketState<B>) {
    let op = &state.ops[state.idx];
    state.harness.apply(op);
    state.idx = state.idx.wrapping_add(1);
    if state.idx == state.ops.len() {
        state.idx = 0;
    }
}

pub fn calm_market_workload<B: OrderBookApi>() -> Workload<CalmMarketState<B>> {
    Workload {
        name: "calm_market",
        slab_cap: 1 << 14,
        // One full pass of the precomputed stream as warmup. Drains the
        // stream once cleanly so the timed body starts in steady-state.
        warmup_iters: CALM_N_EVENTS,
        setup: setup_calm_market::<B>,
        hot: hot_calm_market::<B>,
    }
}

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
    use calvera_books::orderbook::{OrderBook as OB1, VecConsumer as Vc1};
    use calvera_books::orderbook_2::{OrderBook as OB2, VecConsumer as Vc2};

    /// Same op stream applied to both variants must run without panicking,
    /// and the handle map must end with the expected number of resting
    /// orders. Proves the workload + Harness drive both engines.
    fn drive<B: OrderBookApi>(populate: &[Op], mixed: &[Op]) -> usize {
        let mut h = Harness::<B>::new(1 << 14);
        h.apply_all(populate);
        h.apply_all(mixed);
        h.handles.len()
    }

    #[test]
    fn populate_then_mixed_drives_both_variants() {
        let populate = populate_uniform_ops(20, 2, 10_000, 1);
        let mixed = mixed_workload_ops(0xC0FFEE, 1024, 10_000, 50, 0.7, 100_000);

        let n1 = drive::<OB1<Vc1>>(&populate, &mixed);
        let n2 = drive::<OB2<Vc2>>(&populate, &mixed);

        // Same op stream → same set of orders rests on both engines.
        assert_eq!(
            n1, n2,
            "v1 and v2 should hold the same number of resting orders after the same stream"
        );
        // Sanity: some orders are resting.
        assert!(n1 > 0);
    }

    #[test]
    fn warm_mixed_runs_steady_state_on_both_variants() {
        // 100k op iters * 2 variants — exceeds the slab cap many times over;
        // would `SlabFull` if the workload weren't steady-state.
        let d1 = run_warm(&mixed_workload::<OB1<Vc1>>(), 100_000);
        let d2 = run_warm(&mixed_workload::<OB2<Vc2>>(), 100_000);
        // Sanity only — perf isn't being tested here, just survival.
        assert!(d1 > std::time::Duration::ZERO);
        assert!(d2 > std::time::Duration::ZERO);
    }

    #[test]
    fn warm_matching_workloads_survive_both_variants() {
        // Destructive-matching workloads: many iters must not exhaust the slab
        // (match_single is depth-bounded; sweep refills every SWEEP_STRIPS).
        // 500k iters * 2 variants each — a SlabFull or bad steady-state would
        // panic well before the end.
        for iters in [500_000u64] {
            let _ = run_warm(&match_single_workload::<OB1<Vc1>>(), iters);
            let _ = run_warm(&match_single_workload::<OB2<Vc2>>(), iters);
            let _ = run_warm(&sweep_workload::<OB1<Vc1>>(), iters);
            let _ = run_warm(&sweep_workload::<OB2<Vc2>>(), iters);
        }
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
