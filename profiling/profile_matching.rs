//! Multi-workload profiling harness.
//!
//! One binary, one mode per criterion bench (so flamegraphs line up 1:1 with
//! the numbers in BENCHMARKS.md). Each mode builds its own state once, then
//! hammers the target operation in a tight loop until the wall-clock deadline.
//! No `iter_batched`, no per-iteration setup, no refill arithmetic to drift.
//! `build_book` / `populate` frames stay clearly named so they can be filtered
//! out of the flamegraph.
//!
//! Usage:
//!   cargo build --release -p calvera-books --example profile_matching
//!   target/release/examples/profile_matching <mode> [seconds]
//!
//! Modes (each maps to a criterion bench from BENCHMARKS.md):
//!   rest-single   → limit_rest/single_level         (inserts at one price)
//!   rest-spread   → limit_rest/spread_levels        (inserts across many prices)
//!   match-single  → limit_match_single/full_consume (one full-consume match)
//!   sweep-limit   → limit_sweep_levels              (limit-order multi-level)
//!   sweep-market  → market_sweep                    (market-order multi-level)
//!   cancel        → cancel/mid_book                 (O(1) cancel)

use calvera_books::hmap_book::{MarketOrderMode, OrderBook, OrderId, Price, Side, VecConsumer};
use std::env;
use std::time::{Duration, Instant};

const SLAB_CAP: usize = 1 << 18; // 262_144
const MID: u64 = 1_000_000;

// Same book shape as the criterion benches: VecConsumer accumulates each
// fill into a Vec<Fill>. The profiler clears the Vec between iterations
// (see modes below) so it doesn't grow unboundedly across the 12s run.
type Book = OrderBook<VecConsumer>;

/// Bitmask for the deadline-check throttle. We only call `Instant::now()` when
/// `(tick & POLL_MASK) == 0`, i.e. once every 4096 iterations. Without this,
/// `mach_absolute_time` dominates the profile for fast ops (~70% of samples
/// in rest-single before this fix). Overshoot at deadline is bounded by
/// 4096 × per-op cost (microseconds even at slow ops; negligible).
const POLL_MASK: u64 = 0xfff;

fn usage() -> ! {
    eprintln!("usage: profile_matching <mode> [seconds]");
    eprintln!();
    eprintln!("  rest-single   inserts at a single price level (depth grows)");
    eprintln!("  rest-spread   inserts spread across many distinct prices");
    eprintln!("  match-single  one full-consume match per call");
    eprintln!("  sweep-limit   limit-order sweep across many levels");
    eprintln!("  sweep-market  market-order sweep across many levels");
    eprintln!("  cancel        O(1) cancel of resting orders");
    std::process::exit(1);
}

fn main() {
    let mut args = env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| usage());
    let secs: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let deadline = Instant::now() + Duration::from_secs(secs);

    eprintln!("mode={} duration={}s", mode, secs);
    match mode.as_str() {
        "rest-single" => profile_rest_single(deadline),
        "rest-spread" => profile_rest_spread(deadline),
        "match-single" => profile_match_single(deadline),
        "sweep-limit" => profile_sweep_limit(deadline),
        "sweep-market" => profile_sweep_market(deadline),
        "cancel" => profile_cancel(deadline),
        _ => usage(),
    }
}

// ---------------------------------------------------------------------------
// rest-single — inserts at ONE price level (criterion: limit_rest/single_level)
// Exercises: slab insert, push to existing level, order_index insert.
// Does NOT exercise: level creation (BTreeSet/HashMap insert for new price).
// ---------------------------------------------------------------------------
fn profile_rest_single(deadline: Instant) {
    let mut book = Book::new(SLAB_CAP);
    let mut oid: u64 = 0;
    let mut ops: u64 = 0;
    let mut rebuilds: u64 = 0;
    let start = Instant::now();
    let mut tick: u64 = 0;
    loop {
        tick = tick.wrapping_add(1);
        if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
            break;
        }
        oid += 1;
        ops += 1;
        // Always the same price — level created on first call, then pushed-to.
        if book
            .add_limit_order(OrderId(oid), Side::Bid, Price(MID - 1), 1)
            .is_err()
        {
            book = Book::new(SLAB_CAP);
            oid = 0;
            rebuilds += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "rest-single:  {} ops in {:.2}s, {:.0} ops/sec, {} rebuilds",
        ops, elapsed, ops as f64 / elapsed, rebuilds
    );
}

// ---------------------------------------------------------------------------
// rest-spread — inserts at MANY distinct prices (criterion: limit_rest/spread_levels)
// Exercises: slab insert, level creation (BTreeSet insert, levels HashMap insert),
//            order_index insert.
// ---------------------------------------------------------------------------
fn profile_rest_spread(deadline: Instant) {
    let mut book = Book::new(SLAB_CAP);
    let mut oid: u64 = 0;
    let mut ops: u64 = 0;
    let mut rebuilds: u64 = 0;
    let start = Instant::now();
    let mut tick: u64 = 0;
    loop {
        tick = tick.wrapping_add(1);
        if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
            break;
        }
        oid += 1;
        ops += 1;
        // Each insert at a new price → level creation every call.
        let price = MID - 1 - (oid % 200_000);
        if book
            .add_limit_order(OrderId(oid), Side::Bid, Price(price), 1)
            .is_err()
        {
            book = Book::new(SLAB_CAP);
            oid = 0;
            rebuilds += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "rest-spread:  {} ops in {:.2}s, {:.0} ops/sec, {} rebuilds",
        ops, elapsed, ops as f64 / elapsed, rebuilds
    );
}

// ---------------------------------------------------------------------------
// match-single — one full-consume match per call
// (criterion: limit_match_single/full_consume)
// ---------------------------------------------------------------------------
fn profile_match_single(deadline: Instant) {
    const DEPTH: u64 = 100_000;
    let mut total: u64 = 0;
    let mut rebuilds: u64 = 0;
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut done = false;
    while !done {
        let mut book = Book::new(SLAB_CAP);
        for i in 1..=DEPTH {
            let _ = book.add_limit_order(OrderId(i), Side::Ask, Price(MID + i), 1);
        }
        rebuilds += 1;
        let mut next = DEPTH + 1;
        loop {
            tick = tick.wrapping_add(1);
            if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
                done = true;
                break;
            }
            let res = book
                .add_market_order(
                    OrderId(next),
                    Side::Bid,
                    1,
                    MarketOrderMode::ImmediateOrCancel,
                )
                .unwrap();
            book.consumer.fills.clear();
            if res.filled_quantity == 0 {
                break;
            }
            next += 1;
            total += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "match-single: {} matches in {:.2}s, {:.0} matches/sec, {} rebuilds",
        total, elapsed, total as f64 / elapsed, rebuilds
    );
}

// ---------------------------------------------------------------------------
// sweep-limit — limit order that sweeps many levels
// (criterion: limit_sweep_levels)
// ---------------------------------------------------------------------------
fn profile_sweep_limit(deadline: Instant) {
    const DEPTH: u64 = 100_000;
    const SWEEP_QTY: u64 = 8;
    let mut total_orders: u64 = 0;
    let mut total_fills: u64 = 0;
    let mut rebuilds: u64 = 0;
    let mut next_oid: u64 = 2 * DEPTH + 1;
    let mut side = Side::Bid;
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut done = false;
    while !done {
        let mut book = Book::new(SLAB_CAP);
        for i in 1..=DEPTH {
            let _ = book.add_limit_order(OrderId(2 * i - 1), Side::Bid, Price(MID - i), 1);
            let _ = book.add_limit_order(OrderId(2 * i), Side::Ask, Price(MID + i), 1);
            book.consumer.fills.clear();
        }
        rebuilds += 1;
        loop {
            tick = tick.wrapping_add(1);
            if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
                done = true;
                break;
            }
            // Aggressive limit price guaranteed to cross all available levels.
            let cross_price = match side {
                Side::Bid => Price(MID + DEPTH),
                Side::Ask => Price(MID - DEPTH),
            };
            let _ = book
                .add_limit_order(OrderId(next_oid), side, cross_price, SWEEP_QTY)
                .unwrap();
            let fills_this_call = book.consumer.fills.len() as u64;
            book.consumer.fills.clear();
            next_oid += 1;
            total_orders += 1;
            total_fills += fills_this_call;
            side = match side {
                Side::Bid => Side::Ask,
                Side::Ask => Side::Bid,
            };
            // Nothing matched — book exhausted on this side.
            if fills_this_call == 0 {
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "sweep-limit:  {} orders, {} fills in {:.2}s, {:.0} orders/sec, {} rebuilds",
        total_orders,
        total_fills,
        elapsed,
        total_orders as f64 / elapsed,
        rebuilds
    );
}

// ---------------------------------------------------------------------------
// sweep-market — market order that sweeps many levels (criterion: market_sweep)
// ---------------------------------------------------------------------------
fn profile_sweep_market(deadline: Instant) {
    const DEPTH: u64 = 100_000;
    const SWEEP_QTY: u64 = 8;
    let mut total_orders: u64 = 0;
    let mut rebuilds: u64 = 0;
    let mut next_oid: u64 = 2 * DEPTH + 1;
    let mut side = Side::Bid;
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut done = false;
    while !done {
        let mut book = Book::new(SLAB_CAP);
        for i in 1..=DEPTH {
            let _ = book.add_limit_order(OrderId(2 * i - 1), Side::Bid, Price(MID - i), 1);
            let _ = book.add_limit_order(OrderId(2 * i), Side::Ask, Price(MID + i), 1);
        }
        rebuilds += 1;
        loop {
            tick = tick.wrapping_add(1);
            if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
                done = true;
                break;
            }
            let res = book
                .add_market_order(
                    OrderId(next_oid),
                    side,
                    SWEEP_QTY,
                    MarketOrderMode::ImmediateOrCancel,
                )
                .unwrap();
            next_oid += 1;
            total_orders += 1;
            side = match side {
                Side::Bid => Side::Ask,
                Side::Ask => Side::Bid,
            };
            if res.filled_quantity == 0 {
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "sweep-market: {} orders in {:.2}s, {:.0} orders/sec, {} rebuilds",
        total_orders,
        elapsed,
        total_orders as f64 / elapsed,
        rebuilds
    );
}

// ---------------------------------------------------------------------------
// cancel — O(1) cancel of resting orders (criterion: cancel/mid_book)
// ---------------------------------------------------------------------------
fn profile_cancel(deadline: Instant) {
    const DEPTH: u64 = 200_000;
    let mut total: u64 = 0;
    let mut rebuilds: u64 = 0;
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut done = false;
    while !done {
        let mut book = Book::new(SLAB_CAP);
        for i in 1..=DEPTH {
            let _ = book.add_limit_order(OrderId(i), Side::Bid, Price(MID - i), 1);
        }
        rebuilds += 1;
        for i in 1..=DEPTH {
            tick = tick.wrapping_add(1);
            if (tick & POLL_MASK) == 0 && Instant::now() > deadline {
                done = true;
                break;
            }
            let _ = book.cancel_limit_order(OrderId(i));
            total += 1;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "cancel:       {} cancels in {:.2}s, {:.0} cancels/sec, {} rebuilds",
        total, elapsed, total as f64 / elapsed, rebuilds
    );
}
