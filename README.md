# Calvera Orderbook

[![CI](https://github.com/Calvera-Markets/orderbook/actions/workflows/ci.yml/badge.svg)](https://github.com/Calvera-Markets/orderbook/actions/workflows/ci.yml)
![coverage](badges/coverage.svg)

           ██████╗ █████╗ ██╗    ██╗   ██╗███████╗██████╗  █████╗ 
          ██╔════╝██╔══██╗██║    ██║   ██║██╔════╝██╔══██╗██╔══██╗
          ██║     ███████║██║    ██║   ██║█████╗  ██████╔╝███████║
          ██║     ██╔══██║██║    ╚██╗ ██╔╝██╔══╝  ██╔══██╗██╔══██║
          ╚██████╗██║  ██║███████╗╚████╔╝ ███████╗██║  ██║██║  ██║
           ╚═════╝╚═╝  ╚═╝╚══════╝ ╚═══╝  ╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝

                    ╔══════════════════════════════════╗
                    ║                                  ║
                    ║     ░▒▓  C A L V E R A  ▓▒░      ║
                    ║        ░▒▓  C L O B  ▓▒░         ║
                    ║                                  ║
                    ╚══════════════════════════════════╝

                ░░░░░░░░░░░░░░░    ║ A ║     ████████████████
                  ░░░░░░░░░░░░░    ║ S ║     ██████████████
                   ░░░░░░░░░░░░    ║ K ║     ███████████
                     ░░░░░░░░░░    ╠═══╣     █████████
                       ░░░░░░░░    ║   ║     ███████
                         ░░░░░░    ║ ▴ ║     █████
                           ░░░░    ║ │ ║     ███
                             ░░    ║ │ ║     ██
                              ░    ║ │ ║     █
                              .  ──╢ ◆ ╟──   .
                              ░    ║ │ ║     █
                             ░░    ║ │ ║     ██
                           ░░░░    ║ │ ║     ███
                         ░░░░░░    ║ ▾ ║     █████
                       ░░░░░░░░    ║   ║     ███████
                     ░░░░░░░░░░    ╠═══╣     █████████
                   ░░░░░░░░░░░░    ║ B ║     ███████████
                  ░░░░░░░░░░░░░    ║ I ║     ██████████████
                ░░░░░░░░░░░░░░░    ║ D ║     ████████████████


A ultra-low latency central-limit order book with **6.74 ns to place** on an open level, **4.41 ns to cancel** in place. Price-time FIFO, integer ticks. This crate export purely the core structure and its matching logic. It is not a full macthing engine server: no sockets, no OUCH/ITCH, no WAL. Fills go through a `FillConsumer` binded at the type level so the call inlines.

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

Workload definitions, methodology, and the full table are in [`BENCHMARKS.md`](BENCHMARKS.md). For the change history of this crate check out [`CHANGELOG.md`](CHANGELOG.md).

Apple Silicon. Engine rows are Criterion medians of a warm steady-state loop (mean of three runs): the cost of one `hot` iteration. `sweep` moves more than the others run to run.

| workload | time | one iteration |
|---|---:|---|
| `cancel` | 4.41 ns | cancel one order in the middle of a live level |
| `place` | 6.74 ns | add one order onto a level that already exists |
| `match_single` | 21.51 ns | add, then fully consume the level head |
| `add_spread` | 48.24 ns | open a price level and drain another |

Tape replay, same machine, 2026-08-17. 10 M synthetic A/C/M ops, ~4 k orders live. Throughput is wall clock from `just tape-replay --synthetic 10000000 --no-latency`. The percentiles are per-op `Instant` samples from the same replay with sampling left on. p50 sits on the timer floor, ~42 ns.

| metric | value |
|---|---:|
| throughput | 36.33 M ops/s |
| mean | 27.5 ns/op |
| p99 | 84 ns |
| p99.9 | 208 ns |
