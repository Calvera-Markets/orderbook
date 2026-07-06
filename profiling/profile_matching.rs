//! Profiling harness. One binary, one (workload × variant) per invocation,
//! so the flamegraph lines up 1:1 with the workload name.
//!
//! Reuses the framework's shared `workloads.rs` (single source of truth) —
//! the workload definitions here are byte-identical to what
//! `benches/engine.rs` measures. Time-deadline-driven tight loop instead of
//! criterion sampling.
//!
//! Usage:
//!   cargo build --release -p calvera-books --example profile_matching
//!   target/release/examples/profile_matching <workload> <variant> [seconds]
//!
//! Workloads (matching `benches/engine.rs`):
//!   mixed         — random add+cancel at 50/50, populated book
//!   add_cancel    — alternating add+cancel of one order, allocation/free
//!   add_spread    — FIFO across 256 prices: BTreeSet create+drain every iter
//!   cancel_heavy  — head-cancel + tail-add on a 50-deep single-level queue
//!   match_single  — full-consume match at one level (add tail + cross head)
//!   sweep         — market order draining 8 levels/iter (multi-strip refill)
//!   deep_book     — power-law depth book; sweep + rebuild the near-mid band
//!   calm_market      — M6 scenario: low-vol OU, MM cancel/replace, no jumps
//!   news_event       — M6 scenario: Student-t jumps → far-level activity
//!   illiquid         — M6 scenario: wide spread, thin book, frequent aggressors
//!   opening_auction  — M6 scenario: deep resting book, slab/index growth
//!
//! Variants:
//!   v1 — orderbook
//!   v2 — orderbook_2

#[path = "../workloads.rs"]
mod workloads;

use std::env;
use std::time::{Duration, Instant};

use calvera_books::orderbook::{OrderBook as OB1, VecConsumer as Vc1};
use calvera_books::orderbook_2::{OrderBook as OB2, VecConsumer as Vc2};
use calvera_books::types::SlabAllocator;

use workloads::{
    Workload, add_cancel_workload, add_spread_workload, calm_market_workload,
    cancel_heavy_workload, deep_book_workload, illiquid_workload, match_single_workload,
    mixed_workload, news_event_workload, opening_auction_workload, sweep_workload,
};

/// Only check the deadline every 4096 iters. Without this, `Instant::now()`
/// dominates the flamegraph for fast workloads (was ~70% of samples in the
/// old rest-single profile before this fix was introduced). Overshoot is
/// bounded by 4096 × per-op cost — microseconds even for slow ops.
const POLL_MASK: u64 = 0xfff;

fn usage() -> ! {
    eprintln!("usage: profile_matching <workload> <variant> [seconds]");
    eprintln!();
    eprintln!(
        "  workloads: mixed | add_cancel | add_spread | cancel_heavy | match_single | sweep | deep_book"
    );
    eprintln!("             | calm_market | news_event | illiquid | opening_auction");
    eprintln!("  variants:  v1 | v2");
    eprintln!("  seconds:   default 20");
    std::process::exit(1);
}

fn run<S>(variant: &str, w: Workload<S>, deadline: Instant) {
    let label = format!("{}/{}", variant, w.name);
    let mut state = (w.setup)(w.slab_cap, SlabAllocator::System)
        .expect("SlabAllocator::System never fails");
    // Warmup: untimed, but exists to mirror what `bench_workload` does so
    // the flamegraph reflects steady-state samples, not first-touch cost.
    for _ in 0..w.warmup_iters {
        (w.hot)(&mut state);
    }

    let start = Instant::now();
    let mut ops: u64 = 0;
    let mut tick: u64 = 0;
    loop {
        tick = tick.wrapping_add(1);
        if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
            break;
        }
        (w.hot)(&mut state);
        ops += 1;
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "{}: {} ops in {:.2}s — {:.0} ops/sec ({:.2} ns/op)",
        label,
        ops,
        elapsed,
        ops as f64 / elapsed,
        elapsed * 1e9 / ops as f64,
    );
}

fn main() {
    let mut args = env::args().skip(1);
    let workload = args.next().unwrap_or_else(|| usage());
    let variant = args.next().unwrap_or_else(|| usage());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let deadline = Instant::now() + Duration::from_secs(secs);

    eprintln!("workload={workload} variant={variant} duration={secs}s");

    match (workload.as_str(), variant.as_str()) {
        ("mixed", "v1") => run(&variant, mixed_workload::<OB1<Vc1>>(), deadline),
        ("mixed", "v2") => run(&variant, mixed_workload::<OB2<Vc2>>(), deadline),
        ("add_cancel", "v1") => run(&variant, add_cancel_workload::<OB1<Vc1>>(), deadline),
        ("add_cancel", "v2") => run(&variant, add_cancel_workload::<OB2<Vc2>>(), deadline),
        ("add_spread", "v1") => run(&variant, add_spread_workload::<OB1<Vc1>>(), deadline),
        ("add_spread", "v2") => run(&variant, add_spread_workload::<OB2<Vc2>>(), deadline),
        ("cancel_heavy", "v1") => run(&variant, cancel_heavy_workload::<OB1<Vc1>>(), deadline),
        ("cancel_heavy", "v2") => run(&variant, cancel_heavy_workload::<OB2<Vc2>>(), deadline),
        ("match_single", "v1") => run(&variant, match_single_workload::<OB1<Vc1>>(), deadline),
        ("match_single", "v2") => run(&variant, match_single_workload::<OB2<Vc2>>(), deadline),
        ("sweep", "v1") => run(&variant, sweep_workload::<OB1<Vc1>>(), deadline),
        ("sweep", "v2") => run(&variant, sweep_workload::<OB2<Vc2>>(), deadline),
        ("deep_book", "v1") => run(&variant, deep_book_workload::<OB1<Vc1>>(), deadline),
        ("deep_book", "v2") => run(&variant, deep_book_workload::<OB2<Vc2>>(), deadline),
        ("calm_market", "v1") => run(&variant, calm_market_workload::<OB1<Vc1>>(), deadline),
        ("calm_market", "v2") => run(&variant, calm_market_workload::<OB2<Vc2>>(), deadline),
        ("news_event", "v1") => run(&variant, news_event_workload::<OB1<Vc1>>(), deadline),
        ("news_event", "v2") => run(&variant, news_event_workload::<OB2<Vc2>>(), deadline),
        ("illiquid", "v1") => run(&variant, illiquid_workload::<OB1<Vc1>>(), deadline),
        ("illiquid", "v2") => run(&variant, illiquid_workload::<OB2<Vc2>>(), deadline),
        ("opening_auction", "v1") => {
            run(&variant, opening_auction_workload::<OB1<Vc1>>(), deadline)
        }
        ("opening_auction", "v2") => {
            run(&variant, opening_auction_workload::<OB2<Vc2>>(), deadline)
        }
        _ => usage(),
    }
}
