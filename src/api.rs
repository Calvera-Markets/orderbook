//! The variant-agnostic order-book surface.
//!
//! Every `OrderBook` variant implements `OrderBookApi` so the bench runner,
//! workloads, and parity harness can be generic over the implementation
//! without caring which matcher (or which handle encoding) is underneath.
//!
//! The handle is an associated type, not a shared concrete type: its bit
//! layout is the engine's business (v1 has no side bit, v2 packs the side),
//! and callers treat it opaquely — get one from `add_limit`, hand it back to
//! `cancel`.

use crate::errors::BookResult;
use crate::types::{MarketOrderMode, MarketOrderResult, Price, SlabAllocator, Side};

pub trait OrderBookApi {
    /// Engine-minted, opaque order handle. The encoding stays private to the
    /// implementation; the framework only stores it and hands it back.
    type Handle: Copy;

    /// Construct with `slab_capacity` total slots. Variants that split the
    /// slab per side divide this internally.
    fn new(slab_capacity: usize) -> Self;

    /// Construct with a specific slab allocator. Default impl ignores the
    /// allocator and falls back to `Self::new` — engines that genuinely
    /// support non-System variants override this. The `BookResult` return
    /// lets implementations signal `UnsupportedAllocator` when a requested
    /// strategy isn't available on this platform.
    fn new_with_alloc(slab_capacity: usize, alloc: SlabAllocator) -> BookResult<Self>
    where
        Self: Sized,
    {
        match alloc {
            SlabAllocator::System => Ok(Self::new(slab_capacity)),
            _ => Err(crate::errors::BookError::UnsupportedAllocator),
        }
    }

    /// Add a limit order. Returns the engine-assigned handle for the resting
    /// remainder, or `None` if the order fully filled on entry and nothing
    /// rests.
    fn add_limit(&mut self, side: Side, price: Price, qty: u64)
    -> BookResult<Option<Self::Handle>>;

    /// Add a market order. Never rests, so there is no handle — only the
    /// fill/cancel outcome.
    fn add_market(
        &mut self,
        side: Side,
        qty: u64,
        mode: MarketOrderMode,
    ) -> BookResult<MarketOrderResult>;

    /// Cancel a resting order by its handle.
    fn cancel(&mut self, handle: Self::Handle) -> BookResult<()>;
}
