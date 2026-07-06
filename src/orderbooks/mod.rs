//! Order-book engine variants.
//!
//! Both implement [`crate::api::OrderBookApi`] with identical behaviour (proven
//! by the parity suite) and differ only in slab/handle/matcher layout:
//!
//! - [`orderbook_legacy`] — v1: one slab shared between bids and asks; handle is
//!   `(generation, slab_index)` with no side bit; runtime side branch in the
//!   matcher. Simpler; interchangeable with v2 for shallow books.
//! - [`orderbook_2`] — v2: a slab per side, side-packed handle, and a
//!   const-generic matcher specialised on side. Pulls ahead on deep,
//!   same-side-bursty books (see `BENCHMARKS.md`).

pub mod orderbook_2;
pub mod orderbook_legacy;
