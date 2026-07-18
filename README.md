# Calvera Orderbook

A central-limit order book. Price-time FIFO, integer ticks. The crate export purely the data structure and its matching logic. It is not a full macthing engine server: no sockets, no OUCH/ITCH, no WAL. Fills go through a `FillConsumer` binded at the type level so the call inlines.

```rust
use calvera_books::{OrderBook, Price, Side, VecConsumer};

let mut book = OrderBook::<VecConsumer>::new(1 << 16);
let h = book.add_limit_order(Side::Bid, Price(100), 10)?;
// `None` means the aggressor fully filled and nothing rested.
if let Some(handle) = h {
    book.cancel_limit_order(handle)?;
}
```

The engine mints `OrderHandle` (generation + slab index + side). You hand that back to cancel. Stale handles after recycle or a full fill return `OrderNotFound`.

## Layout

Each side has its own slab. Orders are 32 bytes, two per 64-byte line, so a same-side walk usually gets the next slot for free. The matcher is specialized on side at compile time. Price levels live in a `u64` hashmap; a `BTreeSet` is only walked when the best level dies.

There is no client id inside the book. If you have a `ClOrdID` or a venue `order_id`, map it to the handle outside (that's what the matching-engine crate and `tape_replay` do).

Modify is cancel + add. Market orders are IOC or FOK.

## Tests and benches

```sh
cargo test -p calvera-books
cargo bench -p calvera-books --bench engine
just tape-replay --synthetic 10000000 --no-latency
```

Workloads and the numbers live in [`BENCHMARKS.md`](BENCHMARKS.md). How we got here is in [`CHANGELOG.md`](CHANGELOG.md).

Apple Silicon. `mixed` is criterion (warm, one add or cancel). The tape is 10M synthetic A/C/M, wall clock. P50 on the tape is the `Instant` floor (~42 ns), not a cycle count.

| load | figures |
|---|---|
| `mixed` | 6.3 ns / op |
| tape | 36.3 M ops/s (27.5 ns/op) |
| tape P99 / P99.9 | 84 ns / 208 ns |