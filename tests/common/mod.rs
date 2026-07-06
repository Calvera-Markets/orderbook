//! Shared, variant-agnostic parity suite (M2.5).
//!
//! Both `orderbook` (v1) and `orderbook_2` (v2) implement `OrderBookApi` with
//! identical behaviour, so the behavioural/stress scenarios that used to be
//! copy-pasted between `parity.rs` and `parity_2.rs` (~1230 near-identical
//! lines each) now live here **once**, generic over `B: ParityBook`. The
//! `tests/parity.rs` entry point stamps out a `#[test]` per scenario for each
//! variant via the `parity_suite!` macro.
//!
//! The harness maintains a `logical_id → engine handle` map (and its reverse)
//! so tests can refer to orders by the short, hand-written IDs they pass in
//! `Op::Limit { id, .. }`, and translate `Fill.resting_id` (an engine handle)
//! back to the logical id for assertions.
//!
//! `OrderBookApi` deliberately doesn't expose fills (they're the concrete
//! consumer's business), so the one thing the generic harness can't reach
//! through the trait — the accumulated `VecConsumer` fills — is bridged by the
//! test-only `FillSource` trait, implemented for each concrete book below.

#![allow(dead_code)]

use std::collections::HashMap;
use std::hash::Hash;

use calvera_books::api::OrderBookApi;
use calvera_books::errors::BookError;
use calvera_books::types::{MarketOrderMode, MarketOrderResult, Price, Side};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ---------------------------------------------------------------------------
// Fill inspection — the one capability not on `OrderBookApi`.
// ---------------------------------------------------------------------------

/// Read/clear the fills accumulated by a book's `VecConsumer`. Test-only:
/// production callers observe fills through the consumer directly, but the
/// generic harness needs a variant-agnostic way to snapshot them.
pub trait FillSource: OrderBookApi {
    /// Every accumulated fill as `(resting handle, quantity)`.
    fn fill_snapshot(&self) -> Vec<(Self::Handle, u64)>;
    /// Count of accumulated fills (cheap; avoids the snapshot allocation).
    fn fill_count(&self) -> usize;
    /// Drop all accumulated fills (keeps the consumer's Vec from growing
    /// unboundedly across long stress runs).
    fn clear_fills(&mut self);
}

impl FillSource for calvera_books::orderbook::OrderBook<calvera_books::orderbook::VecConsumer> {
    fn fill_snapshot(&self) -> Vec<(Self::Handle, u64)> {
        self.consumer
            .fills
            .iter()
            .map(|f| (f.resting_id, f.quantity))
            .collect()
    }
    fn fill_count(&self) -> usize {
        self.consumer.fills.len()
    }
    fn clear_fills(&mut self) {
        self.consumer.fills.clear();
    }
}

impl FillSource
    for calvera_books::orderbook_2::OrderBook<calvera_books::orderbook_2::VecConsumer>
{
    fn fill_snapshot(&self) -> Vec<(Self::Handle, u64)> {
        self.consumer
            .fills
            .iter()
            .map(|f| (f.resting_id, f.quantity))
            .collect()
    }
    fn fill_count(&self) -> usize {
        self.consumer.fills.len()
    }
    fn clear_fills(&mut self) {
        self.consumer.fills.clear();
    }
}

/// Convenience alias: a book the parity suite can drive. Adds the `Eq + Hash`
/// bound the reverse handle map needs (both variants' `OrderHandle` derive
/// them) on top of `FillSource`, so scenario functions can just write
/// `<B: ParityBook>`.
pub trait ParityBook: FillSource
where
    Self::Handle: Eq + Hash,
{
}

impl<B: FillSource> ParityBook for B where B::Handle: Eq + Hash {}

// ---------------------------------------------------------------------------
// Op representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Op {
    Limit {
        id: u64,
        side: Side,
        price: u64,
        qty: u64,
    },
    Cancel {
        id: u64,
    },
    MarketIoc {
        side: Side,
        qty: u64,
    },
    MarketFok {
        side: Side,
        qty: u64,
    },
}

// ---------------------------------------------------------------------------
// Normalised observable types — used by tests to `assert_eq!` against
// hand-written expected values (referencing the test-local logical id).
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FillN {
    pub resting_id: u64,
    pub quantity: u64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MarketResN {
    pub filled: u64,
    pub unfilled: u64,
    pub cancelled: bool,
}

fn mr(r: MarketOrderResult, requested: u64) -> MarketResN {
    MarketResN {
        filled: r.filled(requested),
        unfilled: r.remaining,
        cancelled: r.cancelled(),
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct Harness<B: ParityBook>
where
    B::Handle: Eq + Hash,
{
    pub book: B,
    /// Logical-id → engine handle. Populated when `add_limit` returns
    /// `Some(handle)` (i.e. some quantity rested). Aggressor orders that
    /// fully cross have no handle and are not tracked.
    pub handles: HashMap<u64, B::Handle>,
    /// Reverse map: handle → logical id. Used to translate `Fill.resting_id`
    /// (a handle) back to the test-local logical id for assertions.
    pub handle_to_id: HashMap<B::Handle, u64>,
}

impl<B: ParityBook> Harness<B>
where
    B::Handle: Eq + Hash,
{
    pub fn new(cap: usize) -> Self {
        Self {
            book: B::new(cap),
            handles: HashMap::new(),
            handle_to_id: HashMap::new(),
        }
    }

    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Limit {
                id,
                side,
                price,
                qty,
            } => {
                if let Ok(Some(h)) = self.book.add_limit(side, Price(price), qty) {
                    self.handles.insert(id, h);
                    self.handle_to_id.insert(h, id);
                }
            }
            Op::Cancel { id } => {
                if let Some(h) = self.handles.remove(&id) {
                    self.handle_to_id.remove(&h);
                    let _ = self.book.cancel(h);
                }
            }
            Op::MarketIoc { side, qty } => {
                let _ = self
                    .book
                    .add_market(side, qty, MarketOrderMode::ImmediateOrCancel);
            }
            Op::MarketFok { side, qty } => {
                let _ = self.book.add_market(side, qty, MarketOrderMode::FillOrKill);
            }
        }
    }

    pub fn drain_fills(&mut self) {
        self.book.clear_fills();
    }

    pub fn fills(&self) -> Vec<FillN> {
        self.book
            .fill_snapshot()
            .iter()
            .map(|(h, q)| FillN {
                resting_id: self.handle_to_id.get(h).copied().unwrap_or(0),
                quantity: *q,
            })
            .collect()
    }
}

/// Returns the slice of fills produced *during* `f`, translated to logical
/// ids.
fn fills_during<B, F>(p: &mut Harness<B>, f: F) -> Vec<FillN>
where
    B: ParityBook,
    B::Handle: Eq + Hash,
    F: FnOnce(&mut Harness<B>),
{
    let before = p.book.fill_count();
    f(p);
    p.fills()[before..].to_vec()
}

// ---------------------------------------------------------------------------
// Scripted scenarios — one behavioural concern per function. Each becomes a
// `#[test]` per variant via `parity_suite!` in `tests/parity.rs`.
// ---------------------------------------------------------------------------

pub fn empty_book_has_no_fills<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let p = Harness::<B>::new(8);
    assert_eq!(p.fills(), vec![]);
}

pub fn add_resting_bid_produces_no_fills<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(fills, vec![]);
}

pub fn add_resting_ask_produces_no_fills<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 1,
            side: Side::Ask,
            price: 200,
            qty: 5,
        });
    });
    assert_eq!(fills, vec![]);
}

pub fn add_then_cancel_roundtrip<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 5,
    });
    p.apply(Op::Cancel { id: 1 });
    // Subsequent ask at the same price should rest, not cross.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Ask,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(fills, vec![]);
}

pub fn cancel_unknown_order_is_order_not_found<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    // Add an order, capture its handle, cancel it (bumps gen), then cancel the
    // now-stale handle again — must report not found.
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 5,
    });
    let stale = *p.handles.get(&1).unwrap();
    p.apply(Op::Cancel { id: 1 }); // valid cancel → gen bumped on free
    let r = p.book.cancel(stale);
    assert_eq!(r, Err(BookError::OrderNotFound));
}

pub fn crossing_limit_exactly_fills_resting<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 5,
    });
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Bid,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 1,
            quantity: 5
        }]
    );
}

pub fn crossing_limit_partially_fills_resting_remainder_stays<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 10,
    });
    // Aggressor wants 3 — resting 1 keeps qty=7.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Bid,
            price: 100,
            qty: 3,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 1,
            quantity: 3
        }]
    );

    // Next bid at 100 for 4 should fill against the remaining 7.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 3,
            side: Side::Bid,
            price: 100,
            qty: 4,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 1,
            quantity: 4
        }]
    );
}

pub fn crossing_limit_fills_then_rests_remainder<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 3,
    });
    // Aggressor wants 10; eats the 3, then 7 rests as a bid at 100.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Bid,
            price: 100,
            qty: 10,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 1,
            quantity: 3
        }]
    );

    // Confirm the 7 rested on the bid side: a subsequent matching ask should hit it.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 3,
            side: Side::Ask,
            price: 100,
            qty: 7,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 2,
            quantity: 7
        }]
    );
}

pub fn non_crossing_limit_just_rests<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 110,
        qty: 5,
    });
    // Bid below ask — does not cross.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Bid,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(fills, vec![]);
}

pub fn fifo_priority_at_same_price<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    // Sweep all three; FIFO order 1, 2, 3.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Bid,
            price: 100,
            qty: 6,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 2,
                quantity: 2
            },
            FillN {
                resting_id: 3,
                quantity: 2
            },
        ]
    );
}

pub fn walk_multiple_price_levels<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(16);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 101,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Ask,
        price: 102,
        qty: 4,
    });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Bid,
            price: 102,
            qty: 9,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 2,
                quantity: 3
            },
            FillN {
                resting_id: 3,
                quantity: 4
            },
        ]
    );
}

pub fn walk_stops_at_price_limit<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(16);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 101,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Ask,
        price: 102,
        qty: 4,
    });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Bid,
            price: 101,
            qty: 10,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 2,
                quantity: 3
            },
        ]
    );

    // Remainder rested at 101; an ask at 101 for 5 should clear it.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 100,
            side: Side::Ask,
            price: 101,
            qty: 5,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 99,
            quantity: 5
        }]
    );
}

pub fn cancel_head_of_fifo<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 100,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Bid,
        price: 100,
        qty: 4,
    });
    p.apply(Op::Cancel { id: 1 });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Ask,
            price: 100,
            qty: 7,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 2,
                quantity: 3
            },
            FillN {
                resting_id: 3,
                quantity: 4
            },
        ]
    );
}

pub fn cancel_middle_of_fifo<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 100,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Bid,
        price: 100,
        qty: 4,
    });
    p.apply(Op::Cancel { id: 2 });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Ask,
            price: 100,
            qty: 6,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 3,
                quantity: 4
            },
        ]
    );
}

pub fn cancel_tail_of_fifo<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 100,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Bid,
        price: 100,
        qty: 4,
    });
    p.apply(Op::Cancel { id: 3 });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Ask,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 2,
                quantity: 3
            },
        ]
    );
}

pub fn cancel_only_order_at_level_removes_level<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 5,
    });
    p.apply(Op::Cancel { id: 1 });
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Ask,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(fills, vec![]);

    p.apply(Op::Cancel { id: 2 });
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 3,
            side: Side::Bid,
            price: 99,
            qty: 1,
        });
    });
    assert_eq!(fills, vec![]);
}

pub fn best_price_updates_when_level_drains<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 101,
        qty: 2,
    });

    let r = fills_during(&mut p, |p| {
        p.apply(Op::MarketIoc {
            side: Side::Bid,
            qty: 2,
        });
    });
    assert_eq!(
        r,
        vec![FillN {
            resting_id: 1,
            quantity: 2
        }]
    );

    // Next bid at 100 should NOT cross (best ask is now 101).
    let r = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 3,
            side: Side::Bid,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(r, vec![]);

    // Bid at 101 should hit order 2.
    let r = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 4,
            side: Side::Bid,
            price: 101,
            qty: 2,
        });
    });
    assert_eq!(
        r,
        vec![FillN {
            resting_id: 2,
            quantity: 2
        }]
    );
}

pub fn market_ioc_fully_filled<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 5,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 101,
        qty: 5,
    });

    let r = p
        .book
        .add_market(Side::Bid, 7, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(
        mr(r, 7),
        MarketResN {
            filled: 7,
            unfilled: 0,
            cancelled: false
        }
    );
    let r = p
        .book
        .add_market(Side::Bid, 0, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(
        mr(r, 0),
        MarketResN {
            filled: 0,
            unfilled: 0,
            cancelled: false
        }
    );
    assert_eq!(
        p.fills(),
        vec![
            FillN {
                resting_id: 1,
                quantity: 5
            },
            FillN {
                resting_id: 2,
                quantity: 2
            },
        ]
    );
}

pub fn market_ioc_partial_then_cancelled<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 3,
    });
    p.apply(Op::MarketIoc {
        side: Side::Bid,
        qty: 10,
    });
    assert_eq!(
        p.fills(),
        vec![FillN {
            resting_id: 1,
            quantity: 3
        }]
    );
    let r = p
        .book
        .add_market(Side::Bid, 10, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(
        mr(r, 10),
        MarketResN {
            filled: 0,
            unfilled: 10,
            cancelled: true
        }
    );
}

pub fn market_ioc_against_empty_side<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::MarketIoc {
        side: Side::Bid,
        qty: 5,
    });
    assert_eq!(p.fills(), vec![]);
}

pub fn market_fok_succeeds_when_liquid<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 5,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Ask,
        price: 101,
        qty: 5,
    });
    p.apply(Op::MarketFok {
        side: Side::Bid,
        qty: 7,
    });
    assert_eq!(
        p.fills(),
        vec![
            FillN {
                resting_id: 1,
                quantity: 5
            },
            FillN {
                resting_id: 2,
                quantity: 2
            },
        ]
    );
}

pub fn market_fok_kills_when_illiquid<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(8);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 5,
    });
    p.apply(Op::MarketFok {
        side: Side::Bid,
        qty: 10,
    });
    assert_eq!(p.fills(), vec![]);

    // Resting ask must still be there.
    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 2,
            side: Side::Bid,
            price: 100,
            qty: 5,
        });
    });
    assert_eq!(
        fills,
        vec![FillN {
            resting_id: 1,
            quantity: 5
        }]
    );
}

pub fn limit_ask_aggressor_walks_bid_book<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let mut p = Harness::<B>::new(16);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 102,
        qty: 2,
    });
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 101,
        qty: 3,
    });
    p.apply(Op::Limit {
        id: 3,
        side: Side::Bid,
        price: 100,
        qty: 4,
    });

    let fills = fills_during(&mut p, |p| {
        p.apply(Op::Limit {
            id: 99,
            side: Side::Ask,
            price: 100,
            qty: 9,
        });
    });
    assert_eq!(
        fills,
        vec![
            FillN {
                resting_id: 1,
                quantity: 2
            },
            FillN {
                resting_id: 2,
                quantity: 3
            },
            FillN {
                resting_id: 3,
                quantity: 4
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// ABA / generation defence
// ---------------------------------------------------------------------------

pub fn stale_handle_after_recycle_is_rejected<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    // Add → cancel → re-add at the same slab slot. The first handle is
    // stale; cancelling with it must return OrderNotFound, not silently
    // affect the new occupant.
    let mut p = Harness::<B>::new(4);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Bid,
        price: 100,
        qty: 1,
    });
    let stale = *p.handles.get(&1).unwrap();
    p.apply(Op::Cancel { id: 1 }); // bumps generation
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 200,
        qty: 1,
    });
    let fresh = *p.handles.get(&2).unwrap();

    // Stale handle (gen mismatch) is rejected.
    assert_eq!(p.book.cancel(stale), Err(BookError::OrderNotFound));
    // Fresh handle still works.
    assert_eq!(p.book.cancel(fresh), Ok(()));
    // And is also stale after its own cancel.
    assert_eq!(p.book.cancel(fresh), Err(BookError::OrderNotFound));
}

pub fn stale_handle_after_match_is_rejected<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    // An order that gets matched out (not explicitly cancelled) must also
    // bump its generation, so a held handle is invalidated.
    let mut p = Harness::<B>::new(4);
    p.apply(Op::Limit {
        id: 1,
        side: Side::Ask,
        price: 100,
        qty: 1,
    });
    let stale = *p.handles.get(&1).unwrap();
    // Aggressor consumes the resting order — generation bumps during the
    // sweep's freed-chain pass.
    p.apply(Op::Limit {
        id: 2,
        side: Side::Bid,
        price: 100,
        qty: 1,
    });

    assert_eq!(p.book.cancel(stale), Err(BookError::OrderNotFound));
}

// ---------------------------------------------------------------------------
// Randomised stress — no-panic smoke test
// ---------------------------------------------------------------------------

pub fn randomized_stress<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let cap = 4096;
    let mut p = Harness::<B>::new(cap);
    let mut rng = StdRng::seed_from_u64(0xC0FFEE_BEEF);

    let mut live: Vec<u64> = Vec::new();
    let mut next_id: u64 = 1;

    let n_ops = 1500;
    let mut inserts_remaining = cap as i64 - 16;

    for _ in 0..n_ops {
        let roll = rng.random_range(0..100);
        let op = if roll < 55 && inserts_remaining > 0 {
            let id = next_id;
            next_id += 1;
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = rng.random_range(95..=105);
            let qty = rng.random_range(1..=8);
            live.push(id);
            inserts_remaining -= 1;
            Op::Limit {
                id,
                side,
                price,
                qty,
            }
        } else if roll < 80 && !live.is_empty() {
            let pick_real = rng.random_bool(0.9);
            if pick_real {
                let i = rng.random_range(0..live.len());
                let id = live.swap_remove(i);
                Op::Cancel { id }
            } else {
                Op::Cancel {
                    id: rng.random_range(1..next_id.max(2)),
                }
            }
        } else if roll < 90 {
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let qty = rng.random_range(1..=20);
            Op::MarketIoc { side, qty }
        } else {
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let qty = rng.random_range(1..=20);
            Op::MarketFok { side, qty }
        };

        p.apply(op);
    }
}

// ---------------------------------------------------------------------------
// Slab reclamation — cancel must return the slot to the freelist
// ---------------------------------------------------------------------------

pub fn cancel_reclaims_slab_slot<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    let cap = 4;
    let mut p = Harness::<B>::new(cap);
    for i in 0..(cap as u64 * 8) {
        p.apply(Op::Limit {
            id: i,
            side: Side::Bid,
            price: 100,
            qty: 1,
        });
        p.apply(Op::Cancel { id: i });
    }
}

// ---------------------------------------------------------------------------
// Millions-of-ops randomised stress
// ---------------------------------------------------------------------------

pub fn randomized_stress_millions<B: ParityBook>()
where
    B::Handle: Eq + Hash,
{
    const OPS: usize = 2_000_000;
    const CAP: usize = 16_384;
    const TARGET_LIVE: usize = 4096;
    const DRAIN_EVERY: usize = 100_000;
    const SEED: u64 = 0xC0FFEE_F00D_DEAD;

    let mut p = Harness::<B>::new(CAP);
    let mut rng = StdRng::seed_from_u64(SEED);

    let mut live: Vec<u64> = Vec::with_capacity(CAP);
    let mut next_id: u64 = 1;

    let mut n_limit = 0u64;
    let mut n_cancel = 0u64;
    let mut n_ioc = 0u64;
    let mut n_fok = 0u64;

    for i in 0..OPS {
        let load = live.len() as f64 / TARGET_LIVE as f64;
        let limit_cut = if load < 0.5 {
            0.80
        } else if load < 1.0 {
            0.55
        } else if load < 1.5 {
            0.35
        } else {
            0.20
        };
        let cancel_cut = limit_cut + 0.30;
        let ioc_cut = cancel_cut + 0.13;

        let r: f64 = rng.random();

        let op = if r < limit_cut {
            let id = next_id;
            next_id += 1;
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = if rng.random_bool(0.95) {
                rng.random_range(95..=105)
            } else {
                rng.random_range(50..=150)
            };
            let qty = rng.random_range(1..=10);
            live.push(id);
            n_limit += 1;
            Op::Limit {
                id,
                side,
                price,
                qty,
            }
        } else if r < cancel_cut && !live.is_empty() {
            let id = if rng.random_bool(0.9) {
                let idx = rng.random_range(0..live.len());
                live.swap_remove(idx)
            } else {
                rng.random_range(1..next_id.max(2))
            };
            n_cancel += 1;
            Op::Cancel { id }
        } else if r < ioc_cut {
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let qty = rng.random_range(1..=30);
            n_ioc += 1;
            Op::MarketIoc { side, qty }
        } else {
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let qty = rng.random_range(1..=20);
            n_fok += 1;
            Op::MarketFok { side, qty }
        };

        p.apply(op);

        if i % DRAIN_EVERY == DRAIN_EVERY - 1 {
            p.drain_fills();
        }
    }

    eprintln!(
        "randomized_stress_millions: ops={} limits={} cancels={} ioc={} fok={} \
         final_live={} next_id={}",
        OPS,
        n_limit,
        n_cancel,
        n_ioc,
        n_fok,
        live.len(),
        next_id - 1
    );
}
