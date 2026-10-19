//! Per-fill veto seam for the matcher.
//!
//! `add_limit_order_gated` / `add_market_order_gated` ask the gate before
//! each fill. `OpenGate` admits everything and folds away, so the ungated
//! entry points compile to the same loop as before.

use crate::orderbook::OrderHandle;
use crate::types::Price;

/// Resting order the aggressor is about to trade against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerRef {
    pub handle: OrderHandle,
    pub price: Price,
    pub resting: u64,
}

/// Verdict for one prospective fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Trade `fill_qty` against the maker.
    Fill,
    /// Remove the whole maker order with no fill and keep sweeping.
    CancelMaker,
    /// End the sweep; the aggressor's remainder does not rest.
    StopTaker,
}

pub trait MatchGate {
    fn admit(&mut self, maker: MakerRef, fill_qty: u64) -> Admit;
}

/// Admits every fill.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenGate;

impl MatchGate for OpenGate {
    #[inline(always)]
    fn admit(&mut self, _maker: MakerRef, _fill_qty: u64) -> Admit {
        Admit::Fill
    }
}

/// Result of a gated limit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatedOutcome {
    /// Handle of the rested remainder, if any.
    pub handle: Option<OrderHandle>,
    /// Quantity left unfilled after the sweep.
    pub remaining: u64,
    /// The gate stopped the aggressor; `remaining` was not rested.
    pub stopped: bool,
}
