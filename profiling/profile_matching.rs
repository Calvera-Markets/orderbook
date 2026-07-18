//! Profiling harness. One binary, one workload per invocation.
//!
//! Usage:
//!   cargo build --release -p calvera-books --example profile_matching
//!   target/release/examples/profile_matching <workload> [seconds]

#[path = "../workloads.rs"]
mod workloads;

use std::env;
use std::time::{Duration, Instant};

use calvera_books::orderbook::{OrderBook, VecConsumer};
use calvera_books::types::SlabAllocator;

use workloads::{
    Workload, add_cancel_workload, add_spread_workload, calm_market_workload,
    cancel_heavy_workload, deep_book_workload, illiquid_workload, match_single_workload,
    mixed_workload, news_event_workload, opening_auction_workload, sweep_workload,
};

/// Only check the deadline every 4096 iters. Without this, `Instant::now()`
/// dominates the flamegraph for fast workloads.
const POLL_MASK: u64 = 0xfff;

type Book = OrderBook<VecConsumer>;

fn usage() -> ! {
    eprintln!("usage: profile_matching <workload> [seconds]");
    eprintln!();
    eprintln!(
        "  workloads: mixed | add_cancel | add_spread | cancel_heavy | match_single | sweep | deep_book"
    );
    eprintln!("             | calm_market | news_event | illiquid | opening_auction");
    eprintln!("  seconds:   default 20");
    std::process::exit(1);
}

fn run<S>(w: Workload<S>, deadline: Instant) {
    let mut state = (w.setup)(w.slab_cap, SlabAllocator::System)
        .expect("SlabAllocator::System never fails");
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
        w.name,
        ops,
        elapsed,
        ops as f64 / elapsed,
        elapsed * 1e9 / ops as f64,
    );
}

fn main() {
    let mut args = env::args().skip(1);
    let workload = args.next().unwrap_or_else(|| usage());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let deadline = Instant::now() + Duration::from_secs(secs);

    eprintln!("workload={workload} duration={secs}s");

    match workload.as_str() {
        "mixed" => run(mixed_workload::<Book>(), deadline),
        "add_cancel" => run(add_cancel_workload::<Book>(), deadline),
        "add_spread" => run(add_spread_workload::<Book>(), deadline),
        "cancel_heavy" => run(cancel_heavy_workload::<Book>(), deadline),
        "match_single" => run(match_single_workload::<Book>(), deadline),
        "sweep" => run(sweep_workload::<Book>(), deadline),
        "deep_book" => run(deep_book_workload::<Book>(), deadline),
        "calm_market" => run(calm_market_workload::<Book>(), deadline),
        "news_event" => run(news_event_workload::<Book>(), deadline),
        "illiquid" => run(illiquid_workload::<Book>(), deadline),
        "opening_auction" => run(opening_auction_workload::<Book>(), deadline),
        _ => usage(),
    }
}
