//! Shared value types used across all `OrderBook` variants.
//!
//! These are byte-identical across implementations and carry no behavior tied
//! to a particular matcher, so they live here rather than being duplicated per
//! variant module. `OrderHandle` / `SlabIndex` / `Fill` will join them once the
//! handle encoding is unified (M1.2).

/// Fixed-point price. 1 unit = 1 tick of the instrument.
/// All price arithmetic is integer arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

/// Outcome of a market order. Carries only `remaining` (the unfilled
/// quantity); `filled` is a pure function of the requested quantity, which
/// the caller already has in scope.
///
/// Storing `remaining` rather than `filled` matches the matcher's internal
/// variable, so the construction site (`MarketOrderResult { remaining }`)
/// does literally zero arithmetic, and `cancelled()` becomes a free check
/// (`remaining > 0`) that doesn't need the requested quantity passed in.
///
/// `#[repr(transparent)]` makes this ABI-identical to a bare `u64`: returned
/// in a register instead of via `sret`, and `Result<MarketOrderResult,
/// BookError>` fits in 16 bytes (two registers on AArch64) instead of the
/// 32-byte struct that the previous 3-field layout forced through stack
/// memory.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketOrderResult {
    pub remaining: u64,
}

impl MarketOrderResult {
    /// Quantity that was actually filled.
    #[inline(always)]
    pub fn filled(self, requested: u64) -> u64 {
        requested - self.remaining
    }

    /// True if any quantity was cancelled (partial-fill IOC or full FOK kill).
    #[inline(always)]
    pub fn cancelled(self) -> bool {
        self.remaining > 0
    }
}
