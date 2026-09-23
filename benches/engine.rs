//! Generic engine bench. One bench binary, every workload.
//! Workload definitions live in `../workloads.rs`.

#[path = "../workloads.rs"]
mod workloads;

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use calvera_books::orderbook::{OrderBook, VecConsumer};
use calvera_books::types::SlabAllocator;

use workloads::{
    Workload, add_cancel_workload, add_spread_workload, calm_market_workload,
    cancel_heavy_workload, cancel_in_place_workload, deep_book_workload, illiquid_workload,
    match_single_workload, mixed_workload, news_event_workload, opening_auction_workload,
    place_workload, sweep_workload,
};

/// Parse `BENCH_SLAB_CAP` value: accepts plain integers (`16384`) or
/// power-of-two shorthand (`1<<14`).
fn parse_slab_cap(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Some((base, exp)) = s.split_once("<<") {
        let b: usize = base.trim().parse().ok()?;
        let e: u32 = exp.trim().parse().ok()?;
        return b.checked_shl(e);
    }
    s.parse().ok()
}

/// Run a workload through criterion's `iter_custom`. Setup + warmup happen
/// once per benchmark inside the bench closure (untimed); only the inner
/// `iters` calls to `hot` are timed.
///
/// Bench id: `workload[/cap_N][/alloc_K]`.
fn bench_workload<S>(c: &mut Criterion, w: Workload<S>) {
    let slab_cap = std::env::var("BENCH_SLAB_CAP")
        .ok()
        .and_then(|s| parse_slab_cap(&s))
        .unwrap_or(w.slab_cap);
    let alloc = std::env::var("BENCH_SLAB_ALLOC")
        .ok()
        .and_then(|s| SlabAllocator::parse(&s))
        .unwrap_or(SlabAllocator::System);

    let mut id = w.name.to_string();
    if slab_cap != w.slab_cap {
        id.push_str(&format!("/cap_{}", slab_cap));
    }
    if alloc != SlabAllocator::System {
        id.push_str(&format!("/alloc_{}", alloc.slug()));
    }

    let state_result = (w.setup)(slab_cap, alloc);
    let mut state = match state_result {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "skipping {}: setup failed ({:?}). Linux-only allocator on this host?",
                id, e
            );
            return;
        }
    };

    let hot = w.hot;
    let prepare = w.prepare;
    let prepare_batch = w.prepare_batch;
    let warmup_iters = w.warmup_iters;

    c.bench_function(&id, move |b| {
        // `prepare` rests the orders a pure-cancel burst is about to drop.
        // It stays outside the clock. Workloads without it keep the plain loop.
        if let Some(prepare) = prepare {
            let mut left = warmup_iters as u64;
            while left > 0 {
                let n = left.min(prepare_batch);
                prepare(&mut state, n);
                for _ in 0..n {
                    hot(&mut state);
                }
                left -= n;
            }
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                let mut left = iters;
                while left > 0 {
                    let n = left.min(prepare_batch);
                    prepare(&mut state, n);
                    let start = Instant::now();
                    for _ in 0..n {
                        hot(&mut state);
                    }
                    total += start.elapsed();
                    left -= n;
                }
                total
            });
        } else {
            for _ in 0..warmup_iters {
                hot(&mut state);
            }
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    hot(&mut state);
                }
                start.elapsed()
            });
        }
    });
}

fn run(c: &mut Criterion) {
    type Book = OrderBook<VecConsumer>;
    bench_workload(c, mixed_workload::<Book>());
    bench_workload(c, add_cancel_workload::<Book>());
    bench_workload(c, add_spread_workload::<Book>());
    bench_workload(c, cancel_heavy_workload::<Book>());
    bench_workload(c, cancel_in_place_workload::<Book>());
    bench_workload(c, place_workload::<Book>());
    bench_workload(c, match_single_workload::<Book>());
    bench_workload(c, sweep_workload::<Book>());
    bench_workload(c, deep_book_workload::<Book>());
    bench_workload(c, calm_market_workload::<Book>());
    bench_workload(c, news_event_workload::<Book>());
    bench_workload(c, illiquid_workload::<Book>());
    bench_workload(c, opening_auction_workload::<Book>());
}

criterion_group!(engine, run);
criterion_main!(engine);
