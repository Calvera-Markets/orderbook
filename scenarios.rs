//! Scenario layer (M6) — parametric market models that emit event streams.
//!
//! Micro-workloads (`workloads.rs`) isolate single code paths; scenarios stress
//! the engine the way production traffic would — where most events are
//! cancel/replace, the book is dense near BBO and sparse at far levels, and the
//! mid walks (and occasionally jumps) rather than hitting an extreme every iter.
//!
//! Three principles (see `BENCH_FRAMEWORK_PLAN.md` M6):
//!   1. Scenarios produce **events, not API calls** — pure `Vec<Event>` data
//!      with no `OrderBookApi` dependency. One stream replays against every
//!      impl, the profiler, and the parity harness — fair by construction.
//!   2. The stream is precomputed once (in a workload's `setup`) and replayed
//!      in the hot loop; generation cost never lands in the timed body.
//!   3. A small named menu — each scenario is a `(model, params, seed)` tuple.
//!
//! Determinism: RNG is `rand_chacha::ChaCha8Rng` with a pinned seed, so a given
//! `(params, seed)` always yields the same stream regardless of `rand` version,
//! compiler, or target arch. `Event` is a POD `Copy` enum — serialization-ready
//! for future recorded-tape support (bincode/postcard), though the serde derive
//! itself is deferred until that lands (it would pull serde onto the library's
//! value types).
//!
//! Shared `#[path]` infra: consumers (benches, profiler, parity tests) each
//! pull this in and use different subsets, so `dead_code` is expected per-crate.

#![allow(dead_code)]

use std::collections::VecDeque;

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use calvera_books::types::{MarketOrderMode, Side};

// ---------------------------------------------------------------------------
// Event — the canonical scenario output and (future) tape format.
// ---------------------------------------------------------------------------

/// A single market-affecting event with a workload-local logical `id`. Pure
/// data: no engine handle (the engine mints those on replay), no references,
/// `Copy` — POD-like so it serializes cleanly when tape support arrives.
///
/// Mirrors `workloads::Op` deliberately; the scenario layer owns the wire-shape
/// type, and workloads/parity convert `Event → Op` at replay time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    LimitAdd {
        id: u64,
        side: Side,
        price: u64,
        qty: u64,
    },
    Cancel {
        id: u64,
    },
    Market {
        side: Side,
        qty: u64,
        mode: MarketOrderMode,
    },
}

/// A named event-stream generator. `generate` is deterministic in the
/// scenario's seed — call it as many times as you like, same stream out.
pub trait Scenario {
    fn name(&self) -> &'static str;
    fn generate(&self, n_events: usize) -> Vec<Event>;
}

// ---------------------------------------------------------------------------
// Parametric market model
// ---------------------------------------------------------------------------

/// Knobs for the OU + market-maker generator. Treat the per-scenario values as
/// a constitution — pin `(params, seed)`, never tune them to a desired outcome
/// (see the plan's calibration-drift risk).
#[derive(Debug, Clone, Copy)]
pub struct ScenarioParams {
    pub seed: u64,
    /// OU long-run mean / starting mid (ticks).
    pub mid0: f64,
    /// OU mean-reversion rate per step.
    pub theta: f64,
    /// OU innovation std-dev (ticks).
    pub sigma: f64,
    /// Per-step probability of a fat-tailed jump. 0.0 disables jumps.
    pub jump_prob: f64,
    /// Degrees of freedom for the Student-t jump shock (df=3 → heavy tails).
    pub jump_df: u32,
    /// Multiplier on the Student-t sample when a jump fires (ticks).
    pub jump_scale: f64,
    /// Target count of resting market-maker quotes; the generator keeps its
    /// live-quote deque around this depth.
    pub mm_depth: usize,
    /// Each MM quote sits `1..=quote_offsets` ticks away from the current mid.
    pub quote_offsets: u64,
    /// Per-step probability of an aggressor market order (vs an MM quote/cancel
    /// step). ~1/(cancel:trade ratio).
    pub aggressor_rate: f64,
}

/// Standard-normal sample via Box–Muller (one uniform pair → one normal).
fn normal(rng: &mut ChaCha8Rng) -> f64 {
    // Guard u1 == 0 → ln(0) = -inf.
    let mut u1: f64 = rng.random_range(0.0..1.0);
    if u1 == 0.0 {
        u1 = f64::MIN_POSITIVE;
    }
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Student-t sample with `df` degrees of freedom: `Z / sqrt(V/df)` where
/// `Z ~ N(0,1)` and `V ~ ChiSquared(df) = Σ Zᵢ²`. Heavy-tailed for small `df`.
fn student_t(rng: &mut ChaCha8Rng, df: u32) -> f64 {
    let z = normal(rng);
    let mut v = 0.0;
    for _ in 0..df.max(1) {
        let zi = normal(rng);
        v += zi * zi;
    }
    z / (v / df.max(1) as f64).sqrt()
}

/// Core generator: an OU mid with occasional Student-t jumps, driving a
/// market-maker cancel/replace rhythm plus an aggressor market-order stream.
///
/// Cycle-clean: the final `mm_depth` events drain every remaining live quote,
/// so a runner that wraps around the precomputed vec re-enters a consistent
/// state (no unbounded slab growth across long replays). Ids are reused across
/// cycles, so the replaying harness's `id → handle` map stays bounded.
fn generate_events(p: &ScenarioParams, n_events: usize) -> Vec<Event> {
    let mut rng = ChaCha8Rng::seed_from_u64(p.seed);
    let mut events = Vec::with_capacity(n_events);

    let mut next_id: u64 = 100_000;
    let mut live: VecDeque<u64> = VecDeque::with_capacity(p.mm_depth + 1);
    let mut mid = p.mid0;

    // Reserve the tail for the drain so the stream is self-balancing.
    let main_n = n_events.saturating_sub(p.mm_depth);

    while events.len() < main_n {
        // OU step: dX = θ(μ − X) + σε, ε ~ N(0,1), dt = 1.
        mid += p.theta * (p.mid0 - mid) + p.sigma * normal(&mut rng);

        // Fat-tailed jump.
        if p.jump_prob > 0.0 && rng.random_bool(p.jump_prob) {
            mid += p.jump_scale * student_t(&mut rng, p.jump_df);
        }

        let tick = mid.round().max(1.0) as u64;

        // Aggressor market order — doesn't touch the MM deque.
        if rng.random_bool(p.aggressor_rate) {
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            events.push(Event::Market {
                side,
                qty: 1,
                mode: MarketOrderMode::ImmediateOrCancel,
            });
            continue;
        }

        // MM rhythm: keep the deque near `mm_depth`; cancel oldest before the
        // new quote when full.
        if live.len() >= p.mm_depth {
            if let Some(id) = live.pop_front() {
                events.push(Event::Cancel { id });
                if events.len() >= main_n {
                    break;
                }
            }
        }

        next_id += 1;
        let side = if rng.random_bool(0.5) {
            Side::Bid
        } else {
            Side::Ask
        };
        let offset = rng.random_range(1..=p.quote_offsets);
        let price = match side {
            Side::Bid => tick.saturating_sub(offset).max(1),
            Side::Ask => tick.saturating_add(offset),
        };
        events.push(Event::LimitAdd {
            id: next_id,
            side,
            price,
            qty: 1,
        });
        live.push_back(next_id);
    }

    // Drain: cancel every remaining live quote so the stream ends empty.
    while let Some(id) = live.pop_front() {
        if events.len() >= n_events {
            break;
        }
        events.push(Event::Cancel { id });
    }

    events
}

/// A concrete scenario: a named `ScenarioParams` bundle.
pub struct MarketScenario {
    pub name: &'static str,
    pub params: ScenarioParams,
}

impl Scenario for MarketScenario {
    fn name(&self) -> &'static str {
        self.name
    }
    fn generate(&self, n_events: usize) -> Vec<Event> {
        generate_events(&self.params, n_events)
    }
}

// ---------------------------------------------------------------------------
// Named menu. Each is a pinned (params, seed) tuple — a constitution, not a
// tuning surface.
// ---------------------------------------------------------------------------

/// Default baseline: low-vol OU, MM-dominated, no jumps, ~1% aggressor rate.
/// The reference scenario for cancel/replace cost.
pub fn calm_market() -> MarketScenario {
    MarketScenario {
        name: "calm_market",
        params: ScenarioParams {
            seed: 0xCA1_DA7A,
            mid0: 10_000.0,
            theta: 0.05,
            sigma: 0.5,
            jump_prob: 0.0,
            jump_df: 3,
            jump_scale: 0.0,
            mm_depth: 10,
            quote_offsets: 5,
            aggressor_rate: 0.01,
        },
    }
}

/// Fat-tailed jumps punctuate a calm baseline — the mid lurches, dragging MM
/// quotes to far prices. Stresses far-level activity and the BTreeSet price
/// index.
pub fn news_event() -> MarketScenario {
    MarketScenario {
        name: "news_event",
        params: ScenarioParams {
            seed: 0x0DD_BEEF,
            mid0: 10_000.0,
            theta: 0.02, // slower reversion → jumps persist longer
            sigma: 0.5,
            jump_prob: 5e-4,
            jump_df: 3,
            jump_scale: 40.0,
            mm_depth: 10,
            quote_offsets: 5,
            aggressor_rate: 0.02,
        },
    }
}

/// Wide spread, thin book, frequent aggressors. Stresses the HashMap-of-levels
/// at low density and the sparse far-level regime.
pub fn illiquid() -> MarketScenario {
    MarketScenario {
        name: "illiquid",
        params: ScenarioParams {
            seed: 0x1_11_1_9_D,
            mid0: 10_000.0,
            theta: 0.05,
            sigma: 1.0,
            jump_prob: 0.0,
            jump_df: 3,
            jump_scale: 0.0,
            mm_depth: 4,
            quote_offsets: 20,
            aggressor_rate: 0.05,
        },
    }
}

/// Deep resting book (large `mm_depth` → the deque ramps up at stream start
/// before steady state). Stresses the slab allocator and index growth.
pub fn opening_auction() -> MarketScenario {
    MarketScenario {
        name: "opening_auction",
        params: ScenarioParams {
            seed: 0x0_9E_11_9,
            mid0: 10_000.0,
            theta: 0.05,
            sigma: 0.5,
            jump_prob: 0.0,
            jump_df: 3,
            jump_scale: 0.0,
            mm_depth: 50,
            quote_offsets: 8,
            aggressor_rate: 0.01,
        },
    }
}

/// The full named menu, in registry order.
#[allow(dead_code)]
pub fn all() -> Vec<MarketScenario> {
    vec![calm_market(), news_event(), illiquid(), opening_auction()]
}
