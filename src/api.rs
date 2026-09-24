//! The order-book surface used by benches, workloads, and the parity harness.
//!
//! The handle is an associated type: its bit layout is the engine's
//! business, and callers treat it opaquely — get one from `add_limit`,
//! hand it back to `cancel`.

use crate::errors::BookResult;
use crate::types::{MarketOrderMode, MarketOrderResult, Price, Side, SlabAllocator};

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

#[cfg(test)]
mod tests {
    use super::OrderBookApi;
    use crate::errors::BookError;
    use crate::types::{MarketOrderMode, MarketOrderResult, Price, Side, SlabAllocator};

    /// Keeps the trait's default `new_with_alloc`. `OrderBook` overrides it.
    struct Fallback;

    impl OrderBookApi for Fallback {
        type Handle = ();

        fn new(_slab_capacity: usize) -> Self {
            Fallback
        }

        fn add_limit(
            &mut self,
            _side: Side,
            _price: Price,
            _qty: u64,
        ) -> crate::errors::BookResult<Option<Self::Handle>> {
            Ok(None)
        }

        fn add_market(
            &mut self,
            _side: Side,
            _qty: u64,
            _mode: MarketOrderMode,
        ) -> crate::errors::BookResult<MarketOrderResult> {
            Ok(MarketOrderResult { remaining: 0 })
        }

        fn cancel(&mut self, _handle: Self::Handle) -> crate::errors::BookResult<()> {
            Ok(())
        }
    }

    #[test]
    fn default_new_with_alloc_accepts_system_and_rejects_other_allocators() {
        let mut book = Fallback::new_with_alloc(8, SlabAllocator::System).unwrap();
        assert_eq!(book.add_limit(Side::Bid, Price(1), 1).unwrap(), None);
        assert_eq!(
            book.add_market(Side::Ask, 1, MarketOrderMode::ImmediateOrCancel)
                .unwrap()
                .remaining,
            0
        );
        book.cancel(()).unwrap();

        assert!(matches!(
            Fallback::new_with_alloc(8, SlabAllocator::MadvHugepage),
            Err(BookError::UnsupportedAllocator)
        ));
        assert!(matches!(
            Fallback::new_with_alloc(8, SlabAllocator::Hugetlb),
            Err(BookError::UnsupportedAllocator)
        ));
    }
}
