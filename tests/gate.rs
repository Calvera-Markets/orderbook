//! Per-fill veto: `MatchGate` verdicts against a naive price-time model.

use calvera_books::types::{MarketOrderMode, Price, Side};
use calvera_books::{
    Admit, BookError, MakerRef, MatchGate, OpenGate, OrderBook, OrderHandle, VecConsumer,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

type Book = OrderBook<VecConsumer>;

/// Replays a fixed verdict script and records every admit call.
struct ScriptGate {
    script: Vec<Admit>,
    calls: Vec<(MakerRef, u64, Admit)>,
}

impl ScriptGate {
    fn new(script: Vec<Admit>) -> Self {
        Self {
            script,
            calls: Vec::new(),
        }
    }
}

impl MatchGate for ScriptGate {
    fn admit(&mut self, maker: MakerRef, fill_qty: u64) -> Admit {
        let verdict = match self.script.get(self.calls.len()) {
            Some(v) => *v,
            None => Admit::Fill,
        };
        self.calls.push((maker, fill_qty, verdict));
        verdict
    }
}

/// Random verdicts with a fixed seed.
struct RandomGate {
    rng: StdRng,
    calls: Vec<(MakerRef, u64, Admit)>,
}

impl MatchGate for RandomGate {
    fn admit(&mut self, maker: MakerRef, fill_qty: u64) -> Admit {
        let verdict = match self.rng.random_range(0..10u8) {
            0 => Admit::CancelMaker,
            1 => Admit::StopTaker,
            _ => Admit::Fill,
        };
        self.calls.push((maker, fill_qty, verdict));
        verdict
    }
}

fn rest(book: &mut Book, side: Side, price: u64, qty: u64) -> OrderHandle {
    match book.add_limit_order(side, Price(price), qty) {
        Ok(Some(h)) => h,
        other => panic!("expected a resting order, got {other:?}"),
    }
}

fn drain(book: &mut Book) -> Vec<(OrderHandle, u64)> {
    book.consumer
        .fills
        .drain(..)
        .map(|f| (f.resting_id, f.quantity))
        .collect()
}

#[test]
fn open_gate_matches_the_ungated_path() {
    let mut a = Book::new(64);
    let mut b = Book::new(64);
    for (p, q) in [(101, 3), (102, 4), (101, 5)] {
        rest(&mut a, Side::Ask, p, q);
        rest(&mut b, Side::Ask, p, q);
    }
    let ha = a.add_limit_order(Side::Bid, Price(102), 10).unwrap();
    let out = b
        .add_limit_order_gated(Side::Bid, Price(102), 10, &mut OpenGate)
        .unwrap();
    assert_eq!(ha, out.handle);
    assert!(!out.stopped);
    assert_eq!(out.remaining, 0);
    assert_eq!(drain(&mut a), drain(&mut b));
}

#[test]
fn cancel_maker_removes_the_whole_order_and_keeps_sweeping() {
    let mut book = Book::new(64);
    let m1 = rest(&mut book, Side::Ask, 101, 5);
    let m2 = rest(&mut book, Side::Ask, 101, 5);
    let m3 = rest(&mut book, Side::Ask, 102, 5);

    let mut gate = ScriptGate::new(vec![Admit::CancelMaker, Admit::Fill, Admit::Fill]);
    let out = book
        .add_limit_order_gated(Side::Bid, Price(102), 7, &mut gate)
        .unwrap();
    assert!(!out.stopped);
    assert_eq!(out.remaining, 0);
    assert_eq!(out.handle, None);
    assert_eq!(drain(&mut book), vec![(m2, 5), (m3, 2)]);
    assert_eq!(gate.calls[0].0.handle, m1);
    assert_eq!(gate.calls[0].0.resting, 5);

    assert_eq!(book.cancel_limit_order(m1), Err(BookError::OrderNotFound));
    // Only m3's remaining 3 is left.
    let res = book
        .add_market_order(Side::Bid, 100, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(res.remaining, 97);
    assert_eq!(drain(&mut book), vec![(m3, 3)]);
}

#[test]
fn stop_taker_keeps_prior_fills_and_does_not_rest() {
    let mut book = Book::new(64);
    let m1 = rest(&mut book, Side::Bid, 100, 2);
    let m2 = rest(&mut book, Side::Bid, 100, 4);

    let mut gate = ScriptGate::new(vec![Admit::Fill, Admit::StopTaker]);
    let out = book
        .add_limit_order_gated(Side::Ask, Price(99), 10, &mut gate)
        .unwrap();
    assert!(out.stopped);
    assert_eq!(out.handle, None);
    assert_eq!(out.remaining, 8);
    assert_eq!(drain(&mut book), vec![(m1, 2)]);

    // m2 untouched; nothing of the taker rested on the ask side.
    let res = book
        .add_market_order(Side::Ask, 100, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(res.remaining, 96);
    assert_eq!(drain(&mut book), vec![(m2, 4)]);
    let res = book
        .add_market_order(Side::Bid, 100, MarketOrderMode::ImmediateOrCancel)
        .unwrap();
    assert_eq!(res.remaining, 100);
}

#[test]
fn stop_taker_on_a_market_order_reports_stopped() {
    let mut book = Book::new(64);
    rest(&mut book, Side::Ask, 101, 5);
    let mut gate = ScriptGate::new(vec![Admit::StopTaker]);
    let (res, stopped) = book
        .add_market_order_gated(Side::Bid, 3, MarketOrderMode::ImmediateOrCancel, &mut gate)
        .unwrap();
    assert!(stopped);
    assert_eq!(res.remaining, 3);
    assert!(drain(&mut book).is_empty());
}

/// Naive price-time book.
#[derive(Default)]
struct Model {
    seq: u64,
    orders: Vec<(Side, u64, u64, OrderHandle, u64)>,
}

impl Model {
    fn queue(&self, opp: Side) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.orders.len())
            .filter(|i| self.orders[*i].0 == opp)
            .collect();
        idx.sort_by_key(|i| {
            let (_, price, seq, _, _) = self.orders[*i];
            let key = match opp {
                Side::Ask => price,
                Side::Bid => u64::MAX - price,
            };
            (key, seq)
        });
        idx
    }

    /// Apply recorded gate calls; returns expected fills and remainder.
    fn apply(
        &mut self,
        side: Side,
        limit: u64,
        mut remaining: u64,
        calls: &[(MakerRef, u64, Admit)],
    ) -> (Vec<(OrderHandle, u64)>, u64, bool) {
        let opp = match side {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        };
        let mut fills = Vec::new();
        let mut calls = calls.iter();
        let mut stopped = false;
        let mut gone = Vec::new();
        for i in self.queue(opp) {
            if remaining == 0 {
                break;
            }
            let (_, price, _, handle, qty) = self.orders[i];
            let crosses = match side {
                Side::Bid => limit >= price,
                Side::Ask => limit <= price,
            };
            if !crosses {
                break;
            }
            let fill = remaining.min(qty);
            let (maker, fill_qty, verdict) = calls.next().expect("gate call missing");
            assert_eq!(maker.handle, handle);
            assert_eq!(maker.price, Price(price));
            assert_eq!(maker.resting, qty);
            assert_eq!(*fill_qty, fill);
            match verdict {
                Admit::Fill => {
                    remaining -= fill;
                    fills.push((handle, fill));
                    self.orders[i].4 -= fill;
                    if self.orders[i].4 == 0 {
                        gone.push(i);
                    }
                }
                Admit::CancelMaker => gone.push(i),
                Admit::StopTaker => {
                    stopped = true;
                    break;
                }
            }
        }
        assert!(calls.next().is_none(), "extra gate calls");
        gone.sort_unstable();
        for i in gone.into_iter().rev() {
            self.orders.remove(i);
        }
        (fills, remaining, stopped)
    }

    fn rest(&mut self, side: Side, price: u64, handle: OrderHandle, qty: u64) {
        self.seq += 1;
        self.orders.push((side, price, self.seq, handle, qty));
    }
}

#[test]
fn random_verdicts_match_the_reference_model() {
    for seed in 0..64u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        // Small slab so freed slots are reused heavily.
        let mut book = Book::new(32);
        let mut model = Model::default();
        let mut gate = RandomGate {
            rng: StdRng::seed_from_u64(seed ^ 0xA5A5),
            calls: Vec::new(),
        };
        for _ in 0..400 {
            let resting = model.orders.len();
            if resting > 0 && rng.random_range(0..5u8) == 0 {
                let i = rng.random_range(0..resting);
                let handle = model.orders.remove(i).3;
                book.cancel_limit_order(handle).unwrap();
                continue;
            }
            let side = if rng.random_bool(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = rng.random_range(95..=105u64);
            let qty = rng.random_range(1..=8u64);
            gate.calls.clear();
            let out = match book.add_limit_order_gated(side, Price(price), qty, &mut gate) {
                Ok(out) => out,
                Err(BookError::SlabFull) => {
                    // Matching ran; only the rest failed.
                    let (fills, _, _) = model.apply(side, price, qty, &gate.calls);
                    assert_eq!(drain(&mut book), fills, "seed {seed}");
                    continue;
                }
                Err(e) => panic!("seed {seed}: {e:?}"),
            };
            let (fills, remaining, stopped) = model.apply(side, price, qty, &gate.calls);
            assert_eq!(drain(&mut book), fills, "seed {seed}");
            assert_eq!(out.remaining, remaining, "seed {seed}");
            assert_eq!(out.stopped, stopped, "seed {seed}");
            match out.handle {
                Some(h) => {
                    assert!(remaining > 0 && !stopped);
                    model.rest(side, price, h, remaining);
                }
                None => assert!(remaining == 0 || stopped),
            }
        }
        // Final sweep of both sides must see exactly the model's orders.
        for side in [Side::Bid, Side::Ask] {
            let opp = match side {
                Side::Bid => Side::Ask,
                Side::Ask => Side::Bid,
            };
            let expect: Vec<(OrderHandle, u64)> = model
                .queue(opp)
                .into_iter()
                .map(|i| (model.orders[i].3, model.orders[i].4))
                .collect();
            book.add_market_order(side, u64::MAX / 2, MarketOrderMode::ImmediateOrCancel)
                .unwrap();
            assert_eq!(drain(&mut book), expect, "seed {seed}");
        }
    }
}
