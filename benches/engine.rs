//! Generic engine bench. One bench binary, every (impl × workload)
//! combination. Eventually replaces `orderbook*.rs` per-variant bench files
//! (M4.3). Workload definitions live in `../workloads.rs` (single source of
//! truth, shared with tests and the profiler).

#[path = "../workloads.rs"]
mod workloads;

use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};

use calvera_books::orderbooks::orderbook_legacy::{OrderBook as OB1, VecConsumer as Vc1};
use calvera_books::orderbooks::orderbook_2::{OrderBook as OB2, VecConsumer as Vc2};
use calvera_books::types::SlabAllocator;

use workloads::{
    Workload, add_cancel_workload, add_spread_workload, calm_market_workload,
    cancel_heavy_workload, deep_book_workload, illiquid_workload, match_single_workload,
    mixed_workload, news_event_workload, opening_auction_workload, sweep_workload,
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
/// Bench id reflects every non-default axis so cross-axis sweeps don't
/// collide in criterion's baseline cache: `impl/workload[/cap_N][/alloc_K]`.
fn bench_workload<S>(c: &mut Criterion, label: &str, w: Workload<S>) {
    let slab_cap = std::env::var("BENCH_SLAB_CAP")
        .ok()
        .and_then(|s| parse_slab_cap(&s))
        .unwrap_or(w.slab_cap);
    let alloc = std::env::var("BENCH_SLAB_ALLOC")
        .ok()
        .and_then(|s| SlabAllocator::parse(&s))
        .unwrap_or(SlabAllocator::System);

    let mut id = format!("{}/{}", label, w.name);
    if slab_cap != w.slab_cap {
        id.push_str(&format!("/cap_{}", slab_cap));
    }
    if alloc != SlabAllocator::System {
        id.push_str(&format!("/alloc_{}", alloc.slug()));
    }

    // Build state up-front so we can skip the bench cleanly if the
    // allocator isn't supported on this platform.
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

    c.bench_function(&id, move |b| {
        // Soft prefault: real engine work that touches slab pages + warms
        // HashMap buckets we'll hit in the timed body.
        for _ in 0..w.warmup_iters {
            (w.hot)(&mut state);
        }
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                (w.hot)(&mut state);
            }
            start.elapsed()
        });
    });
}

/// Registry of (impl, workload) pairs. Adding a new variant or workload is
/// one line here.
fn run(c: &mut Criterion) {
    bench_workload(c, "v1", mixed_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", mixed_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", add_cancel_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", add_cancel_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", add_spread_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", add_spread_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", cancel_heavy_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", cancel_heavy_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", match_single_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", match_single_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", sweep_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", sweep_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", deep_book_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", deep_book_workload::<OB2<Vc2>>());
    // M6 scenario layer.
    bench_workload(c, "v1", calm_market_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", calm_market_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", news_event_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", news_event_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", illiquid_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", illiquid_workload::<OB2<Vc2>>());
    bench_workload(c, "v1", opening_auction_workload::<OB1<Vc1>>());
    bench_workload(c, "v2", opening_auction_workload::<OB2<Vc2>>());
}

criterion_group!(engine, run);
criterion_main!(engine);
