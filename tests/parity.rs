//! Behavioural and stress parity suite for both order-book variants.
//!
//! The scenarios live once, generic over `B: ParityBook`, in `common/mod.rs`.
//! This file stamps out a `#[test]` per scenario for each variant (`v1` =
//! `orderbook`, `v2` = `orderbook_2`) via the `parity_suite!` macro, so both
//! engines run the identical suite and any divergence surfaces as a failing
//! test. Adding a scenario = one generic fn in `common` + one line in the
//! macro body; adding a variant = one `parity_suite!` invocation.

mod common;

/// Stamp out a `#[test]` per scenario in `common`, bound to `$Book`, inside a
/// module named `$modname`. The scenario list lives in this macro body once and
/// is reused by every variant invocation below.
macro_rules! parity_suite {
    ($modname:ident, $Book:ty) => {
        mod $modname {
            use super::common;

            #[test]
            fn empty_book_has_no_fills() {
                common::empty_book_has_no_fills::<$Book>();
            }
            #[test]
            fn add_resting_bid_produces_no_fills() {
                common::add_resting_bid_produces_no_fills::<$Book>();
            }
            #[test]
            fn add_resting_ask_produces_no_fills() {
                common::add_resting_ask_produces_no_fills::<$Book>();
            }
            #[test]
            fn add_then_cancel_roundtrip() {
                common::add_then_cancel_roundtrip::<$Book>();
            }
            #[test]
            fn cancel_unknown_order_is_order_not_found() {
                common::cancel_unknown_order_is_order_not_found::<$Book>();
            }
            #[test]
            fn crossing_limit_exactly_fills_resting() {
                common::crossing_limit_exactly_fills_resting::<$Book>();
            }
            #[test]
            fn crossing_limit_partially_fills_resting_remainder_stays() {
                common::crossing_limit_partially_fills_resting_remainder_stays::<$Book>();
            }
            #[test]
            fn crossing_limit_fills_then_rests_remainder() {
                common::crossing_limit_fills_then_rests_remainder::<$Book>();
            }
            #[test]
            fn non_crossing_limit_just_rests() {
                common::non_crossing_limit_just_rests::<$Book>();
            }
            #[test]
            fn fifo_priority_at_same_price() {
                common::fifo_priority_at_same_price::<$Book>();
            }
            #[test]
            fn walk_multiple_price_levels() {
                common::walk_multiple_price_levels::<$Book>();
            }
            #[test]
            fn walk_stops_at_price_limit() {
                common::walk_stops_at_price_limit::<$Book>();
            }
            #[test]
            fn cancel_head_of_fifo() {
                common::cancel_head_of_fifo::<$Book>();
            }
            #[test]
            fn cancel_middle_of_fifo() {
                common::cancel_middle_of_fifo::<$Book>();
            }
            #[test]
            fn cancel_tail_of_fifo() {
                common::cancel_tail_of_fifo::<$Book>();
            }
            #[test]
            fn cancel_only_order_at_level_removes_level() {
                common::cancel_only_order_at_level_removes_level::<$Book>();
            }
            #[test]
            fn best_price_updates_when_level_drains() {
                common::best_price_updates_when_level_drains::<$Book>();
            }
            #[test]
            fn market_ioc_fully_filled() {
                common::market_ioc_fully_filled::<$Book>();
            }
            #[test]
            fn market_ioc_partial_then_cancelled() {
                common::market_ioc_partial_then_cancelled::<$Book>();
            }
            #[test]
            fn market_ioc_against_empty_side() {
                common::market_ioc_against_empty_side::<$Book>();
            }
            #[test]
            fn market_fok_succeeds_when_liquid() {
                common::market_fok_succeeds_when_liquid::<$Book>();
            }
            #[test]
            fn market_fok_kills_when_illiquid() {
                common::market_fok_kills_when_illiquid::<$Book>();
            }
            #[test]
            fn limit_ask_aggressor_walks_bid_book() {
                common::limit_ask_aggressor_walks_bid_book::<$Book>();
            }
            #[test]
            fn stale_handle_after_recycle_is_rejected() {
                common::stale_handle_after_recycle_is_rejected::<$Book>();
            }
            #[test]
            fn stale_handle_after_match_is_rejected() {
                common::stale_handle_after_match_is_rejected::<$Book>();
            }
            #[test]
            fn randomized_stress() {
                common::randomized_stress::<$Book>();
            }
            #[test]
            fn cancel_reclaims_slab_slot() {
                common::cancel_reclaims_slab_slot::<$Book>();
            }
            #[test]
            fn randomized_stress_millions() {
                common::randomized_stress_millions::<$Book>();
            }
        }
    };
}

parity_suite!(
    v1,
    calvera_books::orderbook::OrderBook<calvera_books::orderbook::VecConsumer>
);
parity_suite!(
    v2,
    calvera_books::orderbook_2::OrderBook<calvera_books::orderbook_2::VecConsumer>
);
