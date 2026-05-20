use calvera_books::{MarketOrderMode, OrderBook, OrderId, Price, Side};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::SmallRng};

const SLAB_CAP: usize = 1 << 20; // 1,048,576

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pre-populate a book with `levels` price levels per side, `orders_per_level`
/// resting orders at each level, around a mid price of 10_000.
fn populated_book(levels: u64, orders_per_level: u64) -> OrderBook {
    let mut book = OrderBook::new(SLAB_CAP);
    let mut oid: u64 = 0;
    // Bids below mid, asks above mid — non-crossing.
    for i in 1..=levels {
        for _ in 0..orders_per_level {
            oid += 1;
            let _ = book.add_limit_order(OrderId(oid), Side::Bid, Price(10_000 - i), 1);
        }
        for _ in 0..orders_per_level {
            oid += 1;
            let _ = book.add_limit_order(OrderId(oid), Side::Ask, Price(10_000 + i), 1);
        }
    }
    book
}

// ---------------------------------------------------------------------------
// 1. Pure rest — limit orders that never cross (insert-only path)
// ---------------------------------------------------------------------------
fn bench_limit_rest(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_rest");
    g.throughput(Throughput::Elements(1));
    g.bench_function("single_level", |b| {
        b.iter_batched(
            || (OrderBook::new(SLAB_CAP), 0u64),
            |(mut book, mut oid)| {
                oid += 1;
                let _ = book.add_limit_order(black_box(OrderId(oid)), Side::Bid, Price(100), 1);
                book
            },
            BatchSize::SmallInput,
        );
    });
    g.bench_function("spread_levels", |b| {
        b.iter_batched(
            || (OrderBook::new(SLAB_CAP), 0u64),
            |(mut book, mut oid)| {
                oid += 1;
                let p = 100 + (oid % 1000);
                let _ = book.add_limit_order(black_box(OrderId(oid)), Side::Bid, Price(p), 1);
                book
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 2. Limit match — aggressor fully consumes a single resting order
// ---------------------------------------------------------------------------
fn bench_limit_match_single(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_match_single");
    g.throughput(Throughput::Elements(1));
    g.bench_function("full_consume", |b| {
        b.iter_batched(
            || {
                let mut book = OrderBook::new(SLAB_CAP);
                let _ = book.add_limit_order(OrderId(1), Side::Ask, Price(100), 1);
                book
            },
            |mut book| {
                let res = book.add_limit_order(black_box(OrderId(2)), Side::Bid, Price(100), 1);
                (book, res)
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 3. Limit sweep — aggressor crosses N levels (stresses side dispatch in loop)
// ---------------------------------------------------------------------------
fn bench_limit_sweep_levels(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_sweep_levels");
    for &levels in &[4u64, 16, 64, 256] {
        g.throughput(Throughput::Elements(levels));
        g.bench_with_input(BenchmarkId::from_parameter(levels), &levels, |b, &levels| {
            b.iter_batched(
                || populated_book(levels, 1),
                |mut book| {
                    let res = book.add_limit_order(
                        black_box(OrderId(u64::MAX)),
                        Side::Bid,
                        Price(10_000 + levels),
                        levels,
                    );
                    (book, res)
                },
                BatchSize::LargeInput,
            );
        });
    }
    g.finish();
}

// ---------------------------------------------------------------------------
// 4. Market order sweep
// ---------------------------------------------------------------------------
fn bench_market_sweep(c: &mut Criterion) {
    let mut g = c.benchmark_group("market_sweep");
    for &levels in &[4u64, 16, 64, 256] {
        g.throughput(Throughput::Elements(levels));
        g.bench_with_input(BenchmarkId::from_parameter(levels), &levels, |b, &levels| {
            b.iter_batched(
                || populated_book(levels, 1),
                |mut book| {
                    let res = book.add_market_order(
                        black_box(OrderId(u64::MAX)),
                        Side::Bid,
                        levels,
                        MarketOrderMode::ImmediateOrCancel,
                    );
                    (book, res)
                },
                BatchSize::LargeInput,
            );
        });
    }
    g.finish();
}

// ---------------------------------------------------------------------------
// 5. Cancel — O(1) cancel path
// ---------------------------------------------------------------------------
fn bench_cancel(c: &mut Criterion) {
    let mut g = c.benchmark_group("cancel");
    g.throughput(Throughput::Elements(1));
    g.bench_function("mid_book", |b| {
        b.iter_batched(
            || {
                // Pre-build a sizeable book; pick the middle order id to cancel.
                let levels = 100u64;
                let opl = 10u64;
                let book = populated_book(levels, opl);
                let target = OrderId(levels * opl); // somewhere in the middle
                (book, target)
            },
            |(mut book, target)| {
                let res = book.cancel_limit_order(black_box(target));
                (book, res)
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 6. Mixed workload — random adds / cancels in a steady-state book
// ---------------------------------------------------------------------------
fn bench_mixed_workload(c: &mut Criterion) {
    let mut g = c.benchmark_group("mixed_workload");
    let ops_per_iter: u64 = 1024;
    g.throughput(Throughput::Elements(ops_per_iter));
    g.bench_function("random_add_cancel", |b| {
        b.iter_batched(
            || {
                let mut rng = SmallRng::seed_from_u64(0xC0FFEE);
                let book = populated_book(50, 4); // ~400 resting orders to start
                // Pre-generate the op sequence so RNG is not in the timed body.
                let mut ops: Vec<(bool, OrderId, Side, Price, u64)> = Vec::with_capacity(ops_per_iter as usize);
                let mut next_oid: u64 = 100_000;
                for _ in 0..ops_per_iter {
                    let is_add = rng.random_bool(0.7);
                    if is_add {
                        next_oid += 1;
                        let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
                        let offset: u64 = rng.random_range(1..=50);
                        let price = match side {
                            Side::Bid => Price(10_000 - offset),
                            Side::Ask => Price(10_000 + offset),
                        };
                        ops.push((true, OrderId(next_oid), side, price, 1));
                    } else {
                        // Cancel some earlier-placed order id (most will hit, some will miss).
                        let target = OrderId(rng.random_range(1..=next_oid));
                        ops.push((false, target, Side::Bid, Price(0), 0));
                    }
                }
                (book, ops)
            },
            |(mut book, ops)| {
                for (is_add, oid, side, price, qty) in ops {
                    if is_add {
                        let _ = black_box(book.add_limit_order(oid, side, price, qty));
                    } else {
                        let _ = black_box(book.cancel_limit_order(oid));
                    }
                }
                book
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

criterion_group!(
    orderbook,
    bench_limit_rest,
    bench_limit_match_single,
    bench_limit_sweep_levels,
    bench_market_sweep,
    bench_cancel,
    bench_mixed_workload,
);
criterion_main!(orderbook);
