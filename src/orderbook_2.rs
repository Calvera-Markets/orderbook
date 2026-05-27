use std::collections::BTreeSet;
// TODO: Consider using sentinel u32::MAX instead of Zero, that way we avoid dead slot at i=0
use std::num::{NonZeroU32, NonZeroU64};

use crate::errors::{BookError, BookResult};
use crate::u64_map::U64Map;

// Shared value types. `pub use` so `calvera_books::orderbook_2::{Price, Side,
// MarketOrderResult}` keeps resolving for existing call sites.
pub use crate::types::{MarketOrderResult, Price, Side};

pub struct OrderBook<C: FillConsumer> {
    // compiler's existing cmov-style dispatch on the match side arms
    // has better perf than [HalfBook; 2].
    //
    // Each side owns its own `OrderSlab`. With a shared slab, an
    // interleaved bid/ask allocation pattern lays out adjacent slots
    // with mixed sides — a same-side chain walk loads cache lines that
    // are half opposite-side data the matcher never reads. Per-side
    // slabs pack same-side orders into the same lines for ~2× effective
    // bid (or ask) density per cache line on bursty same-side allocs.
    bids: HalfBook,
    asks: HalfBook,
    /// Fill sink. Bound at the type level — `C` is chosen by the binary that
    /// constructs the `OrderBook`. The matcher calls `consumer.on_fill(...)`
    /// once per fill; because `C` is a concrete type and `on_fill` is
    /// `#[inline(always)]`, the call monomorphizes + inlines into the
    /// matching loop. Identical generated code to hardcoding the consumer.
    pub consumer: C,
}

/// What to do with unfilled quantity on a market order.
/// CME products differ: futures use ImmediateOrCancel (partial fill ok),
/// some options use FillOrKill (all-or-nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketOrderMode {
    /// Fill as much as possible; cancel any unfilled remainder. Most common.
    ImmediateOrCancel,
    /// Fill everything or fill nothing. If full quantity unavailable, cancel
    /// and return zero fills.
    FillOrKill,
}

impl<C: FillConsumer + Default> OrderBook<C> {
    /// Construct a book with `slab_capacity` total slots split evenly
    /// between bid and ask slabs (each side gets `slab_capacity / 2`).
    /// For asymmetric workloads use `with_capacities`.
    pub fn new(slab_capacity: usize) -> Self {
        Self::with_consumer(slab_capacity, C::default())
    }

    pub fn with_capacities(bid_capacity: usize, ask_capacity: usize) -> Self {
        Self::with_consumer_capacities(bid_capacity, ask_capacity, C::default())
    }
}

impl<C: FillConsumer> OrderBook<C> {
    pub fn with_consumer(slab_capacity: usize, consumer: C) -> Self {
        let half = slab_capacity / 2;
        Self::with_consumer_capacities(half, half, consumer)
    }

    pub fn with_consumer_capacities(bid_capacity: usize, ask_capacity: usize, consumer: C) -> Self {
        Self {
            bids: HalfBook::with_capacity(Side::Bid, bid_capacity),
            asks: HalfBook::with_capacity(Side::Ask, ask_capacity),
            consumer,
        }
    }

    /// Sweep the opposite side, consuming resting orders until either:
    ///   - `remaining` reaches 0,
    ///   - the book is empty on that side, or
    ///   - `price_limit` is set and the next best opposite no longer crosses it.
    ///
    /// `price_limit = None` → market-order semantics (sweep unconditionally).
    /// `price_limit = Some(p)` → limit-order semantics:
    ///   - Bid aggressor crosses iff `p >= best_ask`
    ///   - Ask aggressor crosses iff `p <= best_bid`
    ///
    /// Matching is structured as nested loops:
    ///   - outer (sweep): iterates over price levels of the opposite side
    ///   - inner (level walk): consumes orders from the head of one level,
    ///     reading only `Order.next` and writing nothing to the slab in the
    ///     full-consume steady state
    ///
    /// Freed orders are collected into a per-level chain (via the already-
    /// linked `Order.next` pointers) and stitched across levels with one
    /// write per level boundary. The final chain is spliced into `free_head`
    /// once when the sweep ends.
    /// Match the aggressor against the opposite side. Specialised on
    /// `OPP_IS_ASK`:
    ///
    /// - `OPP_IS_ASK = true`  → aggressor is **Bid**, opposite is **Asks**.
    /// - `OPP_IS_ASK = false` → aggressor is **Ask**, opposite is **Bids**.
    ///
    /// The const generic exists to eliminate every runtime branch on
    /// side in the inlined body. With `OPP_IS_ASK` known at compile
    /// time:
    /// - `opposite` resolves to a single concrete field offset
    ///   (`self.asks` or `self.bids`), no `csel`.
    /// - `opp_side` resolves to a literal (`Side::Ask` or `Side::Bid`).
    /// - The price-limit crossing comparison resolves to a single
    ///   direction (`>=` or `<=`).
    /// - The per-fill `OrderHandle::new(opp_side, ...)` side-bit shift
    ///   folds to a constant `0` or `0x8000_0000_0000_0000`.
    ///
    /// Profile-driven: the runtime-side version bloated every caller's
    /// stack frame by 48 B and spilled two callee-saved SIMD registers
    /// because the materialised `opposite` ptr and precomputed side bit
    /// stayed alive across the whole inlined body. See
    /// `ideas/per_side_slab_result.md` for the asm analysis.
    #[inline(always)]
    fn match_against<const OPP_IS_ASK: bool>(
        &mut self,
        price_limit: Option<Price>,
        quantity: u64,
    ) -> BookResult<u64> {
        let mut remaining = quantity;
        let mut freed_chain_head: Option<SlabIndex> = None;
        let mut freed_chain_tail: Option<SlabIndex> = None;

        // Side of the resting orders the matcher emits handles for.
        // Const after monomorphisation.
        let opp_side: Side = if OPP_IS_ASK { Side::Ask } else { Side::Bid };

        // Field offset known at compile time — no `csel`.
        let opposite: &mut HalfBook = if OPP_IS_ASK {
            &mut self.asks
        } else {
            &mut self.bids
        };

        'sweep: while remaining > 0 {
            // 1) Best price on the opposite side.
            let fill_price = match opposite.best_price {
                Some(p) => p,
                None => break,
            };

            // 2) Limit-order stop condition: bail if our price no longer crosses.
            //    Direction is const after monomorphisation: Bid aggressor
            //    (opp_is_ask) crosses when `limit >= ask`, Ask aggressor
            //    when `limit <= bid`.
            if let Some(limit) = price_limit {
                let crosses = if OPP_IS_ASK {
                    limit >= fill_price
                } else {
                    limit <= fill_price
                };
                if !crosses {
                    break;
                }
            }

            // 3) Walk this level. Inner scope so the `&mut PriceLevel` borrow
            //    ends before we mutate sibling fields of the HalfBook below
            //    (levels.remove, price_index.remove, update_best_price).
            //
            //    `level_freed` carries the per-level freed chain out of the
            //    inner block. We avoid an Option for `level_last_freed` (and
            //    its per-iteration `is_none()` branch) by initializing it to
            //    the level head and only reading it when `consumed_count > 0`.
            let mut consumed_count: u32 = 0;
            let mut level_freed: Option<(SlabIndex, SlabIndex)> = None;
            let level_drained: bool;
            {
                // Split borrow: `levels` and `slab` are sibling fields of
                // `*opposite`. The inner loop reads + mutates the slab
                // while `level` mutably borrows one entry of `levels`.
                let slab = &mut opposite.slab;
                let level_opt = opposite.levels.get_mut(&fill_price);
                let level = match level_opt {
                    Some(l) => l,
                    None => break 'sweep, // best_price desync — defensive
                };
                let head = match level.head {
                    Some(i) => i,
                    None => break 'sweep, // empty level — defensive
                };
                let mut current_idx = head;
                let mut level_last_freed: SlabIndex = head; // only valid when consumed_count > 0
                let mut consumed_qty: u64 = 0;

                level_drained = loop {
                    // Read everything we need from the slot in one shot. After
                    // this we either fully consume (no slab write in this iter)
                    // or partially consume (one slab write at the end).
                    let slot = &slab.slots[current_idx.as_usize()];
                    let resting_qty = slot.quantity;
                    let resting_handle = OrderHandle::new(opp_side, current_idx, slot.generation);
                    let next_idx = slot.next;

                    let fill_qty = remaining.min(resting_qty);
                    remaining -= fill_qty;
                    self.consumer.on_fill(Fill {
                        resting_id: resting_handle,
                        quantity: fill_qty,
                    });

                    if fill_qty == resting_qty {
                        // Full consume. Donate to this level's freed chain
                        // (no link write — already linked via `Order.next`).
                        // Bump the slot's generation so any stale handle held
                        // by a consumer fails the cancel-time check.
                        slab.bump_generation(current_idx);
                        consumed_qty += fill_qty;
                        consumed_count += 1;
                        level_last_freed = current_idx;

                        // Two cases — folded along `next_idx`, not on the
                        // 2x2 of (remaining == 0, next_idx). The
                        // `next_idx == None` cases were identical anyway.
                        match next_idx {
                            Some(n) => {
                                if remaining == 0 {
                                    // Satisfied; `n` is the new head. Clear
                                    // its stale `prev` (was pointing at the
                                    // now-freed `current_idx`) and settle
                                    // level counters once.
                                    slab.slots[n.as_usize()].prev = None;
                                    level.head = Some(n);
                                    level.quantity -= consumed_qty;
                                    level.order_count -= consumed_count;
                                    break false;
                                }
                                current_idx = n;
                            }
                            None => {
                                // Level drained. Whether `remaining` is 0 or
                                // not, the outer sweep handles it: if 0 we
                                // exit; if not, we move to the next best
                                // price.
                                level.head = None;
                                level.tail = None;
                                level.quantity = 0;
                                level.order_count = 0;
                                break true;
                            }
                        }
                    } else {
                        // Partial consume. This is always the terminator —
                        // `remaining` is now 0. `current_idx` survives as the
                        // new head; its `prev` may still point to a now-freed
                        // slot, so clear it unconditionally (cheaper than
                        // branching on whether anything was freed earlier).
                        let s = &mut slab.slots[current_idx.as_usize()];
                        s.quantity -= fill_qty;
                        s.prev = None;
                        consumed_qty += fill_qty;
                        level.head = Some(current_idx);
                        level.quantity -= consumed_qty;
                        level.order_count -= consumed_count;
                        break false;
                    }
                };

                if consumed_count > 0 {
                    // `head` is the level's original head — i.e. the first
                    // order we fully consumed. `level_last_freed` is the
                    // most recent.
                    level_freed = Some((head, level_last_freed));
                }
            }

            // 4) Stitch this level's freed chain into the sweep's freed chain.
            //    Within a level the chain is already linked via `Order.next`,
            //    so we only need one slab write to bridge across levels.
            if let Some((lf_head, lf_tail)) = level_freed {
                match freed_chain_tail {
                    Some(prev_tail) => {
                        opposite.slab.slots[prev_tail.as_usize()].next = Some(lf_head);
                    }
                    None => {
                        freed_chain_head = Some(lf_head);
                    }
                }
                freed_chain_tail = Some(lf_tail);
            }

            // 5) If we fully drained the level, remove it from the half-book.
            if level_drained {
                opposite.levels.remove(&fill_price);
                opposite.price_index.remove(&fill_price);
                if opposite.best_price == Some(fill_price) {
                    opposite.update_best_price();
                }
            }
        }

        // 6) Splice the sweep's freed chain into the opposite slab's
        //    freelist. One write to the chain tail's `next`, one update
        //    to `free_head` on the same half-book. `opposite` is still
        //    the hoisted borrow from the top of the function.
        if let Some(tail) = freed_chain_tail {
            let head = freed_chain_head.expect("invariant: head ⇒ tail");
            opposite.slab.slots[tail.as_usize()].next = opposite.slab.free_head;
            opposite.slab.free_head = Some(head);
        }

        Ok(remaining)
    }

    // (1) Match against the opposite side up to `price`
    // (2) Rest any unfilled remainder on our own side
    //     (a) Alloc slot in the own-side slab, write the order fields
    //     (b) Update price level + slab links via the own HalfBook
    //     (c) Return the engine-assigned `OrderHandle` to the caller
    pub fn add_limit_order(
        &mut self,
        side: Side,
        price: Price,
        quantity: u64,
    ) -> BookResult<Option<OrderHandle>> {
        // Limit semantics: stop matching when price no longer crosses.
        // Dispatch once on aggressor side and select the matcher
        // monomorph that has the opposite side baked in.
        let remaining = match side {
            Side::Bid => self.match_against::<true>(Some(price), quantity)?,
            Side::Ask => self.match_against::<false>(Some(price), quantity)?,
        };

        // Dispatch on side at the call site so each arm operates on a
        // concrete sibling field (self.bids / self.asks) rather than on
        // a runtime-materialised `&mut HalfBook`. Constructing the
        // `OrderHandle` inside each arm with a literal side lets the
        // side-bit shift fold to a constant — no `csel`, no early
        // materialisation, no stack spill for the side bit.
        //
        // The asm diff showed that the previous "let own = match side
        // { ... }" pattern grew the stack frame by 48 B and spilled two
        // SIMD registers because `own` and the precomputed side-bit had
        // to be kept alive across the whole function. This shape keeps
        // dispatch at the leaves where the optimiser can see through.
        let handle = if remaining > 0 {
            match side {
                Side::Bid => {
                    let (order_idx, generation) = self.bids.alloc_and_rest(price, remaining)?;
                    Some(OrderHandle::new(Side::Bid, order_idx, generation))
                }
                Side::Ask => {
                    let (order_idx, generation) = self.asks.alloc_and_rest(price, remaining)?;
                    Some(OrderHandle::new(Side::Ask, order_idx, generation))
                }
            }
        } else {
            None
        };

        self.consumer.flush();
        Ok(handle)
    }

    // (1) Decode handle → (side, idx, generation); reject stale handles
    //     via the per-slot generation check (no hashmap probe)
    // (2) Read price from the slot (still needed to find the level)
    // (3) Unlink from price level (stitch prev/next, drop level if empty)
    // (4) Return slot to freelist (bumps generation, invalidating the handle)
    pub fn cancel_limit_order(&mut self, handle: OrderHandle) -> BookResult<()> {
        let idx = handle.idx();
        let generation = handle.generation();
        let side = handle.side();

        let own: &mut HalfBook = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };

        let slot = &own.slab.slots[idx.as_usize()];
        if slot.generation != generation {
            return Err(BookError::OrderNotFound);
        }
        let price = slot.price;

        own.remove_order_from_l2_book_and_update_slab_links(idx, price);
        own.slab.free(idx);

        Ok(())
    }

    // (1) Inserts order in slab
    // (2) Updates price limit and links
    // (3) Updates order index map
    pub fn add_market_order(
        &mut self,
        side: Side,
        quantity: u64,
        mode: MarketOrderMode,
    ) -> BookResult<MarketOrderResult> {
        // NOTE: Considering the frquency of this happenening, it
        // seems like a big price to pay to run this upfront. I think the
        // best is to actually let it fail iteratively as it tries to fill
        if mode == MarketOrderMode::FillOrKill {
            // NOTE: this match seems superflous, instead of match
            // we can have an bool 0 or 1, and then we index
            // such that struct[0] = asks and struct[1] = bids, thus
            // removing the branching
            let available = match side {
                Side::Bid => self
                    .asks
                    .price_index
                    .iter()
                    .filter_map(|p| self.asks.levels.get(p))
                    .map(|l| l.quantity)
                    .sum::<u64>(),
                Side::Ask => self
                    .bids
                    .price_index
                    .iter()
                    .filter_map(|p| self.bids.levels.get(p))
                    .map(|l| l.quantity)
                    .sum::<u64>(),
            };

            if available < quantity {
                // Insufficient liquidity — cancel entire order, zero fills.
                return Ok(MarketOrderResult {
                    remaining: quantity,
                });
            }
        }

        // Market = sweep unconditionally → price_limit = None.
        // Same per-side dispatch pattern as add_limit_order.
        let remaining = match side {
            Side::Bid => self.match_against::<true>(None, quantity)?,
            Side::Ask => self.match_against::<false>(None, quantity)?,
        };

        self.consumer.flush();
        Ok(MarketOrderResult { remaining })
    }
}

pub struct HalfBook {
    side: Side,
    levels: U64Map<Price, PriceLevel>,
    price_index: BTreeSet<Price>, // sorted; only walked on level drain
    best_price: Option<Price>,
    // Each `HalfBook` owns the slab for its side. With sufficient
    // same-side burstiness (MM stacks, replenishment after sweeps),
    // adjacent slab indices belong to the same side and share cache
    // lines under the 32-byte Order layout — ~2× line density vs a
    // shared slab where bid/ask slots interleave.
    slab: OrderSlab,
}

impl HalfBook {
    fn with_capacity(side: Side, capacity: usize) -> Self {
        Self {
            side,
            levels: U64Map::default(),
            price_index: BTreeSet::new(),
            best_price: None,
            slab: OrderSlab::with_capacity(capacity),
        }
    }

    /// Alloc a fresh slot in this side's slab, write the order fields,
    /// and link it into the side's L2 book. Returns the `(idx, gen)`
    /// pair the caller needs to build the `OrderHandle`.
    ///
    /// Marked `#[inline]` so add_limit_order's per-arm call site sees
    /// the full body and the optimiser can hoist common subexpressions
    /// across the alloc + write + push sequence.
    #[inline]
    fn alloc_and_rest(
        &mut self,
        price: Price,
        quantity: u64,
    ) -> BookResult<(SlabIndex, NonZeroU32)> {
        let (idx, generation) = self.slab.alloc_slot()?;
        let slot = self.slab.get_mut(idx);
        slot.price = price;
        slot.quantity = quantity;
        slot.prev = None;
        slot.next = None;
        self.push_order_to_l2_book_and_update_slab_links(idx);
        Ok((idx, generation))
    }

    /// (1) Update price level and stich slab links
    /// (2) Remove price level if empty (-price_index)
    /// (3) Refresh best price
    fn remove_order_from_l2_book_and_update_slab_links(&mut self, idx: SlabIndex, price: Price) {
        // Split borrow: `levels` and `slab` are sibling fields of `self`,
        // so the borrow checker permits separate mutable borrows.
        let slab = &mut self.slab;
        if let Some(level) = self.levels.get_mut(&price) {
            level.remove(idx, slab);

            if level.is_empty() {
                self.levels.remove(&price);
                self.price_index.remove(&price);

                // Refresh the best price if needed
                if self.best_price == Some(price) {
                    self.update_best_price();
                };
            }
        }
    }

    fn update_best_price(&mut self) {
        self.best_price = match self.side {
            Side::Bid => self.price_index.iter().next_back().copied(),
            Side::Ask => self.price_index.iter().next().copied(),
        };
    }

    /// (1) Get or insert price level (+price index)
    /// (2) Push order to price level and update links
    /// (3) Refresh new best
    fn push_order_to_l2_book_and_update_slab_links(&mut self, idx: SlabIndex) {
        // Split borrow: bind each sibling field separately so the
        // `or_insert_with` closure (which captures `price_index`) does
        // not conflict with the `levels.entry(...)` mutable borrow.
        let slab = &mut self.slab;
        let levels = &mut self.levels;
        let price_index = &mut self.price_index;
        let price = slab.get(idx).price;

        // Get or insert level
        let level = levels.entry(price).or_insert_with(|| {
            // Add new price set
            price_index.insert(price);

            // Insert new price level
            PriceLevel::new(price)
        });

        // Update price level and links
        level.push_order_to_l2_level_and_update_slab_links(idx, slab);

        // Check if it's the new best
        let is_new_best = match self.best_price {
            None => true,
            Some(best) => match self.side {
                Side::Bid => price > best,
                Side::Ask => price < best,
            },
        };
        if is_new_best {
            self.best_price = Some(price);
        }
    }
}

pub struct PriceLevel {
    pub price: Price,
    pub quantity: u64,
    pub head: Option<SlabIndex>,
    pub tail: Option<SlabIndex>,
    pub order_count: u32,
}

impl PriceLevel {
    pub fn new(price: Price) -> Self {
        Self {
            price,
            quantity: 0,
            head: None,
            tail: None,
            order_count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    // Remove arbitrary order (useful for cancel)
    // Say there are three orders:
    // A <- B <- C
    // If we remove B we have to stich the graph:
    // A <- C
    pub fn remove(&mut self, idx: SlabIndex, slab: &mut OrderSlab) {
        let (prev, next, qty) = {
            let o = slab.get(idx);
            (o.prev, o.next, o.quantity)
        };

        // Patch the previous order's `next` pointer.
        match prev {
            // Stich previous to next
            Some(p_idx) => slab.get_mut(p_idx).next = next,
            // If no previous it means its the head, therefore update price level head
            None => self.head = next, // was the head
        };

        // Patch the next order's `prev` pointer.
        match next {
            // Stich next to previous
            Some(n_idx) => slab.get_mut(n_idx).prev = prev,
            // If no next it means its the tail, therefore update price level tail
            None => self.tail = prev,
        };

        // Update quantity and count
        self.quantity -= qty;
        self.order_count -= 1;
    }

    pub fn push_order_to_l2_level_and_update_slab_links(
        &mut self,
        idx: SlabIndex,
        slab: &mut OrderSlab,
    ) {
        let order = slab.get_mut(idx);

        // Fill the order vertices
        order.prev = self.tail; // new tail is formed, and pointing to the previous tail

        if let Some(tail_idx) = self.tail {
            // Reach out to previous tail and have it point to the new tail
            slab.get_mut(tail_idx).next = Some(idx);
        } else {
            // Queue was empty; new order is also the head.
            self.head = Some(idx);
        }

        // Update the tail of the price level
        self.tail = Some(idx);

        // Register quantity and counts
        self.quantity += slab.get(idx).quantity;
        self.order_count += 1;
    }
}

/// Slab allocator. Slots store `Order` directly.
///
/// When a slot is on the freelist, its `Order.next` field is repurposed
/// as the freelist link; `price`, `quantity`, `prev`, `side` are garbage.
/// The `generation` field is the one part of an `Order` that remains
/// meaningful while the slot is free — it's bumped on `free()` so any
/// stale `OrderHandle` held by an external consumer fails the cancel-time
/// generation check (ABA defence).
///
/// Callers must not read a free slot's matching fields — invariant
/// maintained by the matcher and by the generation check in
/// `cancel_limit_order`.
pub struct OrderSlab {
    slots: Vec<Order>,
    free_head: Option<SlabIndex>,
    capacity: usize,
}

impl OrderSlab {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "slab capacity must be non-zero");

        let total_slots = capacity + 1;
        let mut slots = Vec::with_capacity(total_slots);

        // SAFETY: 1 is non-zero. Used as the starting generation for every
        // slot — first alloc returns gen=1, subsequent allocs see the
        // bumped value from the previous free.
        let gen_one = unsafe { NonZeroU32::new_unchecked(1) };

        // slots[0]: reserved sentinel slot, never reachable from any
        // SlabIndex.
        slots.push(Order {
            price: Price(0),
            quantity: 0,
            prev: None,
            next: None,
            generation: gen_one,
            _pad: [0; 4],
        });

        // slots[1..=capacity]: freelist 1 -> 2 -> … -> capacity -> None.
        for i in 1..=capacity {
            let next_free = if i < capacity {
                Some(SlabIndex::new((i + 1) as u32))
            } else {
                None
            };
            // Placeholder `Order` — only `next` (the freelist link) and
            // `generation` (ABA defence) are meaningful while the slot is
            // free.
            slots.push(Order {
                price: Price(0),
                quantity: 0,
                prev: None,
                next: next_free,
                generation: gen_one,
                _pad: [0; 4],
            });
        }

        Self {
            slots,
            free_head: Some(SlabIndex::new(1)),
            capacity,
        }
    }

    #[inline(always)]
    pub fn get(&self, idx: SlabIndex) -> &Order {
        &self.slots[idx.as_usize()]
    }

    #[inline(always)]
    pub fn get_mut(&mut self, idx: SlabIndex) -> &mut Order {
        &mut self.slots[idx.as_usize()]
    }

    /// Bump the generation of the given slot. Called at every slot-recycle
    /// point (cancel via `free`, full-consume during matching) so any
    /// outstanding `OrderHandle` for the previous occupant fails the
    /// cancel-time check. ABA defence.
    ///
    /// Generation lives in 31 bits — the top bit of the handle's
    /// generation field is the side flag. Wraps from `0x7FFF_FFFF → 1`,
    /// skipping `0` to keep the `NonZeroU32` invariant.
    #[inline(always)]
    pub fn bump_generation(&mut self, idx: SlabIndex) {
        let slot = &mut self.slots[idx.as_usize()];
        let next = slot.generation.get().wrapping_add(1) & OrderHandle::GEN_MASK;
        let new_gen = if next == 0 { 1 } else { next };
        // SAFETY: new_gen is non-zero by the branch above, and `& GEN_MASK`
        // keeps it within 31 bits.
        slot.generation = unsafe { NonZeroU32::new_unchecked(new_gen) };
    }

    /// Return `idx` to the freelist, bumping its generation. Any handle
    /// that referenced the previous occupant will now fail the
    /// cancel-time check.
    pub fn free(&mut self, idx: SlabIndex) {
        self.bump_generation(idx);
        // common subexpression elimination (CSE) plus bounds-check elision
        // re-borrowing is free, LLVM optimizes it away
        let slot = &mut self.slots[idx.as_usize()];
        slot.next = self.free_head;
        self.free_head = Some(idx);
    }

    /// Claim a slot from the freelist. Returns `(idx, generation)` — the
    /// caller must build an `OrderHandle` from these and overwrite all of
    /// `price`, `quantity`, `prev`, `next`, `side` via `get_mut` before
    /// any other code reads the slot. `generation` is the value the slot
    /// currently carries (set by the previous `free`).
    pub fn alloc_slot(&mut self) -> BookResult<(SlabIndex, NonZeroU32)> {
        let idx = self.free_head.ok_or(BookError::SlabFull)?;
        // The slot's `next` field holds the freelist link while free; read
        // it before the caller overwrites the slot.
        let next_free = self.slots[idx.as_usize()].next;
        self.free_head = next_free;
        let generation = self.slots[idx.as_usize()].generation;
        Ok((idx, generation))
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// A resting limit order.
///
/// Mechanical Sympathy: Layout is exactly 32 bytes
/// With 64 B cache lines that means two `Order`s per line.
///
/// `#[repr(C, align(32))]` pins both the field layout and the struct
/// alignment to 32, so a `Vec<Order>` is guaranteed to start at a 32-byte
/// boundary and every random index lands in exactly one cache line — no
/// straddle, regardless of `Vec` size or allocator.
///
/// Layout (32 B total): `price` (8) + `quantity` (8) + `prev` (4) +
/// `next` (4) + `generation` (4) + `_pad` (4).
///
/// `side` is no longer stored on the slot. Each `HalfBook` owns its own
/// `OrderSlab`, so the side an order belongs to is implicit in which slab
/// it lives in. The `OrderHandle` carries a side bit, and cancel routes
/// to the correct half-book by reading that bit directly — no slot read
/// required for routing. This frees a byte; whether it returns as
/// additional pad (current) or a future field is a layout decision the
/// 32-byte cache-line commitment outlasts.
#[repr(C, align(32))]
#[derive(Debug, Clone)]
pub struct Order {
    pub price: Price,
    pub quantity: u64, // remaining quantity (decremented on partial fill)
    // Intrusive linked list pointers within a price level's FIFO queue.
    // None = this is the head (prev) or tail (next) of the queue.
    // These are slab indices, not raw pointers — safe to store in a Vec.
    pub prev: Option<SlabIndex>, // order ahead in queue (older, higher priority)
    pub next: Option<SlabIndex>, // order behind in queue (newer, lower priority)
    // Bumped on free() and on full-consume during matching. The
    // cancel-time check rejects any handle whose generation no longer
    // matches the slot's — ABA defence.
    pub generation: NonZeroU32,
    _pad: [u8; 4],
}

const _: () = assert!(std::mem::size_of::<Order>() == 32);
const _: () = assert!(std::mem::align_of::<Order>() == 32);

/// Engine-assigned, opaque order handle.
///
/// Packed `[side: 1][generation: 31][slab_index: 32]`:
/// - bit 63: side (0 = Bid, 1 = Ask)
/// - bits 32..63: generation (31 bits, NonZero)
/// - bits 0..32: slab index (NonZeroU32)
///
/// Side moved onto the handle (was: read from `Order.side` on the slot)
/// so cancel can route to the correct per-side slab with no slot read.
/// Each `HalfBook` owns its own `OrderSlab` — slab ownership *is* the
/// side. Generation gives up its top bit; 2³¹ generations per slot is
/// still far beyond what ABA defence needs. The outer `NonZeroU64` lets
/// `Option<OrderHandle>` niche-pack to 8 bytes.
///
/// Generation mask: producers (`bump_generation`) must keep generation
/// in `[1, 0x7FFF_FFFF]`. Consumers (`OrderHandle::generation`) mask the
/// top bit on read so a fabricated handle with a side bit set doesn't
/// leak into the generation value.
///
/// # Provenance is part of the safety contract
///
/// Handles must only come from `OrderBook::add_limit_order` — they are
/// "permissioned" in the sense that the engine is the sole legitimate
/// issuer. `OrderHandle::new` is technically `pub` so tests and
/// engine-internal code (the matcher constructing fill events, the
/// slab returning newly-allocated handles) can build them, but
/// production callers outside the engine should never call it directly.
/// Treat the type as opaque: get one from `add_limit_order`, hand it
/// back to `cancel_limit_order`, optionally read its `as_u64()` to put
/// on a wire — but don't reverse-engineer the bits or fabricate values.
///
/// The engine relies on this contract to keep the ABA-defence check on
/// `cancel_limit_order` sound. The three failure modes that are NOT
/// caught by the generation check, and why the contract matters:
///
/// 1. **Generation wraparound.** After 2³¹ frees of the *same* slot,
///    gen cycles through `1..=0x7FFF_FFFF` and returns to a
///    previously-issued value. Practically unreachable — even at one
///    free per nanosecond per slot that's ~68 years per slot.
///
/// 2. **Out-of-range `idx`.** If a caller manufactures or corrupts a
///    handle with `idx > slab.capacity`, the slab indexing in
///    `cancel_limit_order` panics before the generation check runs.
///    Failure mode for malformed handles is therefore "panic," not
///    "OrderNotFound." Fine for opaque, engine-issued handles.
///
/// 3. **Never-allocated slot.** All slots are initialised with
///    `gen = 1`. A fabricated handle with `gen = 1` for an unallocated
///    `idx` passes the gen check and then `free` corrupts the freelist.
///    Same root cause as (2): the safety property is "the engine
///    controls all handles," not "any 8 bytes are safe to pass."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderHandle(NonZeroU64);

impl OrderHandle {
    /// Bit position of the side flag inside the packed u64.
    const SIDE_BIT: u64 = 1 << 63;
    /// Mask covering the 31-bit generation field once shifted into place.
    const GEN_MASK: u32 = 0x7FFF_FFFF;

    #[inline(always)]
    pub fn new(side: Side, idx: SlabIndex, generation: NonZeroU32) -> Self {
        // generation must fit in 31 bits — enforced by `bump_generation`,
        // which wraps at 0x7FFF_FFFF. Debug-only check; producers always
        // satisfy this.
        debug_assert!(
            generation.get() <= Self::GEN_MASK,
            "generation must fit in 31 bits (top bit reserved for side)"
        );
        let side_bit = (side as u64) << 63;
        let packed = side_bit | ((generation.get() as u64) << 32) | (idx.0.get() as u64);
        // SAFETY: idx is NonZeroU32, so the low 32 bits are non-zero,
        // making the whole packed value non-zero regardless of side.
        Self(unsafe { NonZeroU64::new_unchecked(packed) })
    }

    #[inline(always)]
    pub fn idx(self) -> SlabIndex {
        let lo = self.0.get() as u32;
        // SAFETY: constructed from a NonZeroU32 in the low half.
        SlabIndex(unsafe { NonZeroU32::new_unchecked(lo) })
    }

    #[inline(always)]
    pub fn generation(self) -> NonZeroU32 {
        // Mask off the side bit before reading generation.
        let hi = ((self.0.get() >> 32) as u32) & Self::GEN_MASK;
        // SAFETY: producers ensure generation is in [1, 0x7FFF_FFFF].
        unsafe { NonZeroU32::new_unchecked(hi) }
    }

    #[inline(always)]
    pub fn side(self) -> Side {
        if self.0.get() & Self::SIDE_BIT != 0 {
            Side::Ask
        } else {
            Side::Bid
        }
    }

    /// Wire-friendly raw value. Useful for FIX-37-style external IDs.
    #[inline(always)]
    pub fn as_u64(self) -> u64 {
        self.0.get()
    }
}

/// Index into the slab.
///
/// Wraps `NonZeroU32` so the compiler can niche-optimise `Option<SlabIndex>`
/// to 4 bytes — `0` becomes the `None` bit pattern. Without this, the
/// `Option` discriminant + alignment padding would cost an extra 4 bytes
/// per field, blowing `Order` past the 32-byte cache-line-friendly size.
///
/// The slab reserves index 0 (allocates `capacity + 1` slots and leaves
/// `slots[0]` permanently unused) so the constraint that `SlabIndex(0)`
/// cannot exist is invisible to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlabIndex(pub NonZeroU32);

impl SlabIndex {
    /// Construct a `SlabIndex` from a raw `u32`. Panics if `n == 0`.
    /// Use in cold setup paths — the matcher only consumes pre-built
    /// indices and never calls this on a hot loop iteration.
    #[inline]
    pub fn new(n: u32) -> Self {
        Self(NonZeroU32::new(n).expect("SlabIndex(0) is reserved (niche-optimisation sentinel)"))
    }

    #[inline(always)]
    pub fn as_usize(self) -> usize {
        self.0.get() as usize
    }
}

#[derive(Debug)]
pub struct Fill {
    pub resting_id: OrderHandle,
    pub quantity: u64,
}

pub trait FillConsumer {
    fn on_fill(&mut self, fill: Fill);

    /// Called by the matcher at the end of every public operation
    /// (`add_limit_order`, `add_market_order`), after all of that
    /// operation's fills have been delivered via `on_fill`. Consumers that
    /// buffer fills internally (e.g. a batched ring-buffer publisher) use
    /// this as their commit point. Default is a no-op — the per-fill
    /// `on_fill` path is the only thing that matters for consumers that
    /// don't batch (Vec, immediate-publish).
    #[inline(always)]
    fn flush(&mut self) {}
}

/// Collects every fill into a `Vec<Fill>` owned by the consumer.
#[derive(Default)]
pub struct VecConsumer {
    pub fills: Vec<Fill>,
}
impl FillConsumer for VecConsumer {
    #[inline(always)]
    fn on_fill(&mut self, fill: Fill) {
        self.fills.push(fill);
    }
}
