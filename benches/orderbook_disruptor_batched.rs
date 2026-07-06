//! v3 benches: per-operation batched disruptor publishing.
//!
//! Same workloads as v1 (Vec) and v2 (single-publish Disruptor). The
//! consumer buffers fills locally during a matcher operation, then issues
//! one `batch_publish(N, ...)` call when the matcher calls `flush()` at
//! the end of the operation.
//!
//! Per-op batching adds zero latency: every fill is visible to the
//! consumer no later than the matcher's `add_*_order` call returns. Cross-
//! operation batching (waiting for N fills across multiple ops) would
//! amortize better but trade latency for throughput — wrong choice for an
//! HFT matcher.
//!
//! Expected cost per fill ≈ Vec::push (~3 ns) + amortized batch slot store
//! (~3 ns) ≈ 5–10 ns/fill — meaningfully cheaper than v2's per-fill
//! publish (~15–20 ns) for any sweep ≥ 16 fills.
//!
//! Consumer axis (not yet in the generic framework): `BatchedDisruptorConsumer`
//! isn't `Default`, so `OrderBook<BatchedDisruptorConsumer>` doesn't satisfy the
//! `OrderBookApi` blanket impl (which requires `C: Default`) and can't ride the
//! generic `benches/engine.rs` runner yet. Until the consumer axis lands, this
//! stays a standalone bench driving the engine's inherent methods directly. It
//! tracks engine-assigned `OrderHandle`s in a local `logical-id → handle` map,
//! same pattern as `workloads::Harness` / the parity suite.

use std::collections::HashMap;
use std::hint::black_box;

use calvera::{BusySpin, Producer, UniConsumerBarrier, UniProducer, build_uni_producer_unchecked};
use calvera_books::orderbooks::orderbook_legacy::{
    Fill, FillConsumer, MarketOrderMode, OrderBook, OrderHandle, Price, Side,
};
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use rand::{Rng, SeedableRng, rngs::SmallRng};

const SLAB_CAP: usize = 1 << 20; // 1,048,576
const RING_CAP: usize = 8192; // power of two; larger than any single op's fill count
const BUFFER_CAP: usize = 256; // sized for the largest sweep in the bench suite

// ---------------------------------------------------------------------------
// BatchedDisruptorConsumer — buffers fills per-op, batch-publishes on flush.
// ---------------------------------------------------------------------------

#[derive(Default)]
#[repr(C)]
struct FillEvent {
    resting_id: u64,
    quantity: u64,
}

struct BatchedDisruptorConsumer {
    producer: UniProducer<FillEvent, UniConsumerBarrier>,
    /// Per-operation accumulator. Pre-sized for the worst-case sweep so
    /// the steady-state path is zero allocations. `flush()` clears len
    /// but keeps capacity.
    buffer: Vec<Fill>,
}

impl BatchedDisruptorConsumer {
    fn new() -> Self {
        let factory = || FillEvent::default();
        let processor = |e: &FillEvent, _seq: calvera::Sequence, _eob: bool| {
            std::hint::black_box(e.resting_id);
            std::hint::black_box(e.quantity);
        };
        let producer = build_uni_producer_unchecked(RING_CAP, factory, BusySpin)
            .handle_events_with(processor)
            .build();
        Self {
            producer,
            buffer: Vec::with_capacity(BUFFER_CAP),
        }
    }
}

impl FillConsumer for BatchedDisruptorConsumer {
    #[inline(always)]
    fn on_fill(&mut self, fill: Fill) {
        // Cheap path: just stash. The Vec is pre-sized so this never
        // reallocates in the benches' workload range (max sweep = 256).
        self.buffer.push(fill);
    }

    #[inline(always)]
    fn flush(&mut self) {
        let n = self.buffer.len();
        if n == 0 {
            return;
        }
        // Borrow split: take the buffer as a slice for the closure to read,
        // while the closure also gets mut access to the producer's slots.
        let buf = &self.buffer;
        self.producer.batch_publish(n, |iter| {
            for (slot, fill) in iter.zip(buf.iter()) {
                // Engine-assigned handle → wire-friendly u64 (packed
                // generation + slab index).
                slot.resting_id = fill.resting_id.as_u64();
                slot.quantity = fill.quantity;
            }
        });
        self.buffer.clear();
    }
}

type Book = OrderBook<BatchedDisruptorConsumer>;

fn fresh_book() -> Book {
    Book::with_consumer(SLAB_CAP, BatchedDisruptorConsumer::new())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Populate `levels` price levels per side, `orders_per_level` orders each,
/// around a 10,000 mid. Resting orders don't cross, so nothing fills.
fn populated_book(levels: u64, orders_per_level: u64) -> Book {
    let mut book = fresh_book();
    for i in 1..=levels {
        for _ in 0..orders_per_level {
            let _ = book.add_limit_order(Side::Bid, Price(10_000 - i), 1);
        }
        for _ in 0..orders_per_level {
            let _ = book.add_limit_order(Side::Ask, Price(10_000 + i), 1);
        }
    }
    book
}

/// Same as `populated_book`, but also returns the engine-assigned handles in
/// insertion order. Handle at index `k` corresponds to logical order id
/// `k + 1` (matching the old `OrderId(oid)` numbering), so cancels can target
/// a specific resting order.
fn populated_book_tracked(levels: u64, orders_per_level: u64) -> (Book, Vec<OrderHandle>) {
    let mut book = fresh_book();
    let mut handles = Vec::with_capacity((levels * orders_per_level * 2) as usize);
    for i in 1..=levels {
        for _ in 0..orders_per_level {
            if let Ok(Some(h)) = book.add_limit_order(Side::Bid, Price(10_000 - i), 1) {
                handles.push(h);
            }
        }
        for _ in 0..orders_per_level {
            if let Ok(Some(h)) = book.add_limit_order(Side::Ask, Price(10_000 + i), 1) {
                handles.push(h);
            }
        }
    }
    (book, handles)
}

// ---------------------------------------------------------------------------
// 1. Pure rest — no fills, no batching effect
// ---------------------------------------------------------------------------
fn bench_limit_rest(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_rest");
    g.throughput(Throughput::Elements(1));
    g.bench_function("single_level", |b| {
        b.iter_batched(
            fresh_book,
            |mut book| {
                let _ = book.add_limit_order(Side::Bid, black_box(Price(100)), 1);
                book
            },
            BatchSize::SmallInput,
        );
    });
    g.bench_function("spread_levels", |b| {
        b.iter_batched(
            || (fresh_book(), 0u64),
            |(mut book, mut n)| {
                n += 1;
                let p = 100 + (n % 1000);
                let _ = book.add_limit_order(Side::Bid, black_box(Price(p)), 1);
                book
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 2. Limit match — 1 fill, batch of 1 (worst-case for batching overhead)
// ---------------------------------------------------------------------------
fn bench_limit_match_single(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_match_single");
    g.throughput(Throughput::Elements(1));
    g.bench_function("full_consume", |b| {
        b.iter_batched(
            || {
                let mut book = fresh_book();
                let _ = book.add_limit_order(Side::Ask, Price(100), 1);
                book
            },
            |mut book| {
                let res = book.add_limit_order(Side::Bid, black_box(Price(100)), 1);
                (book, res)
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// 3. Limit sweep — N fills per op, batch of N
// ---------------------------------------------------------------------------
fn bench_limit_sweep_levels(c: &mut Criterion) {
    let mut g = c.benchmark_group("limit_sweep_levels");
    for &levels in &[4u64, 16, 64, 256] {
        g.throughput(Throughput::Elements(levels));
        g.bench_with_input(
            BenchmarkId::from_parameter(levels),
            &levels,
            |b, &levels| {
                b.iter_batched(
                    || populated_book(levels, 1),
                    |mut book| {
                        let res = book.add_limit_order(
                            Side::Bid,
                            Price(10_000 + levels),
                            black_box(levels),
                        );
                        (book, res)
                    },
                    BatchSize::LargeInput,
                );
            },
        );
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
        g.bench_with_input(
            BenchmarkId::from_parameter(levels),
            &levels,
            |b, &levels| {
                b.iter_batched(
                    || populated_book(levels, 1),
                    |mut book| {
                        let res = book.add_market_order(
                            Side::Bid,
                            black_box(levels),
                            MarketOrderMode::ImmediateOrCancel,
                        );
                        (book, res)
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    g.finish();
}

// ---------------------------------------------------------------------------
// 5. Cancel — no fills, no batching effect
// ---------------------------------------------------------------------------
fn bench_cancel(c: &mut Criterion) {
    let mut g = c.benchmark_group("cancel");
    g.throughput(Throughput::Elements(1));
    g.bench_function("mid_book", |b| {
        b.iter_batched(
            || {
                let levels = 100u64;
                let opl = 10u64;
                let (book, handles) = populated_book_tracked(levels, opl);
                // Old target was OrderId(levels * opl) = the 1000th order added
                // (mid-book); handle index is that id minus one.
                let target = handles[(levels * opl - 1) as usize];
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
// 6. Mixed workload
// ---------------------------------------------------------------------------
fn bench_mixed_workload(c: &mut Criterion) {
    let mut g = c.benchmark_group("mixed_workload");
    let ops_per_iter: u64 = 1024;
    g.throughput(Throughput::Elements(ops_per_iter));
    g.bench_function("random_add_cancel", |b| {
        b.iter_batched(
            || {
                let mut rng = SmallRng::seed_from_u64(0xC0FFEE);
                let (book, populate_handles) = populated_book_tracked(50, 4);
                // Logical id → handle. Populate orders are ids 1..=len; mixed
                // adds continue from 100_000. Cancels that miss the map (never
                // issued / already gone) no-op — same as the old OrderId path.
                let handle_map: HashMap<u64, OrderHandle> =
                    (1u64..).zip(populate_handles).collect();
                // Precompute the op stream (logical ids, not handles).
                let mut ops: Vec<(bool, u64, Side, Price, u64)> =
                    Vec::with_capacity(ops_per_iter as usize);
                let mut next_id: u64 = 100_000;
                for _ in 0..ops_per_iter {
                    let is_add = rng.random_bool(0.7);
                    if is_add {
                        next_id += 1;
                        let side = if rng.random_bool(0.5) {
                            Side::Bid
                        } else {
                            Side::Ask
                        };
                        let offset: u64 = rng.random_range(1..=50);
                        let price = match side {
                            Side::Bid => Price(10_000 - offset),
                            Side::Ask => Price(10_000 + offset),
                        };
                        ops.push((true, next_id, side, price, 1));
                    } else {
                        let target = rng.random_range(1..=next_id);
                        ops.push((false, target, Side::Bid, Price(0), 0));
                    }
                }
                (book, handle_map, ops)
            },
            |(mut book, mut handle_map, ops)| {
                for (is_add, id, side, price, qty) in ops {
                    if is_add {
                        if let Ok(Some(h)) = black_box(book.add_limit_order(side, price, qty)) {
                            handle_map.insert(id, h);
                        }
                    } else if let Some(h) = handle_map.remove(&id) {
                        let _ = black_box(book.cancel_limit_order(h));
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
