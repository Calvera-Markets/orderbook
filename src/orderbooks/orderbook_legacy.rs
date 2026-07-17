use std::collections::BTreeSet;
// TODO: Consider using sentinel u32::MAX instead of Zero, that way we avoid dead slot at i=0
use std::num::{NonZeroU32, NonZeroU64};

use crate::errors::{BookError, BookResult};
pub use crate::types::{MarketOrderMode, MarketOrderResult, Price, Side, SlabAllocator};
use crate::u64_map::U64Map;

pub struct OrderBook<C: FillConsumer> {
    // compiler's existing cmov-style dispatch on the match side arms
    // has better perf than [HalfBook; 2]
    bids: HalfBook,
    asks: HalfBook,
    slab: OrderSlab,
    /// Fill sink. Bound at the type level — `C` is chosen by the binary that
    /// constructs the `OrderBook`. The matcher calls `consumer.on_fill(...)`
    /// once per fill; because `C` is a concrete type and `on_fill` is
    /// `#[inline(always)]`, the call monomorphizes + inlines into the
    /// matching loop. Identical generated code to hardcoding the consumer.
    pub consumer: C,
}

impl<C: FillConsumer + Default> OrderBook<C> {
    pub fn new(slab_capacity: usize) -> Self {
        Self::with_consumer(slab_capacity, C::default())
    }
}

impl<C: FillConsumer> OrderBook<C> {
    pub fn with_consumer(slab_capacity: usize, consumer: C) -> Self {
        Self {
            bids: HalfBook::new(Side::Bid),
            asks: HalfBook::new(Side::Ask),
            slab: OrderSlab::with_capacity(slab_capacity),
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
    #[inline(always)]
    fn match_against_opposite(
        &mut self,
        side: Side,
        price_limit: Option<Price>,
        quantity: u64,
    ) -> BookResult<u64> {
        // TODO: This function needs a cleanup
        // Runtime `side` select below: do not cmov it.
        // A live opposite-ptr across this inlined body spilled SIMD regs.
        let mut remaining = quantity;
        let mut freed_chain_head: Option<SlabIndex> = None;
        let mut freed_chain_tail: Option<SlabIndex> = None;

        'sweep: while remaining > 0 {
            // 1) Best price on the opposite side.
            let best_opposite = match side {
                Side::Bid => self.asks.best_price,
                Side::Ask => self.bids.best_price,
            };
            // Rare miss; the branch *skips* the sweep. Leave it.
            let fill_price = match best_opposite {
                Some(p) => p,
                None => break,
            };

            // 2) Limit-order stop condition: bail if our price no longer crosses.
            //    Candidate for a cmov-into-done flag if a profile shows this
            //    flipping near the touch. Far-from-touch and market (`None`)
            //    predict well.
            if let Some(limit) = price_limit {
                let crosses = match side {
                    Side::Bid => limit >= fill_price,
                    Side::Ask => limit <= fill_price,
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
                let level_opt = match side {
                    Side::Bid => self.asks.levels.get_mut(&fill_price),
                    Side::Ask => self.bids.levels.get_mut(&fill_price),
                };
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
                    let slot = &self.slab.slots[current_idx.as_usize()];
                    let resting_qty = slot.quantity;
                    let resting_handle = OrderHandle::new(current_idx, slot.generation);
                    let next_idx = slot.next;

                    let fill_qty = remaining.min(resting_qty); // cmov
                    remaining -= fill_qty;
                    self.consumer.on_fill(Fill {
                        resting_id: resting_handle,
                        quantity: fill_qty,
                    });

                    // Full vs partial: data-dependent. Sweeps are mostly
                    // full-consume (predictor is fine). Only de-branch if a
                    // flamegraph pins this compare.
                    if fill_qty == resting_qty {
                        // Full consume. Donate to this level's freed chain
                        // (no link write — already linked via `Order.next`).
                        // Bump the slot's generation so any stale handle held
                        // by a consumer fails the cancel-time check.
                        self.slab.bump_generation(current_idx);
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
                                    self.slab.slots[n.as_usize()].prev = None;
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
                        let s = &mut self.slab.slots[current_idx.as_usize()];
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
                        self.slab.slots[prev_tail.as_usize()].next = Some(lf_head);
                    }
                    None => {
                        freed_chain_head = Some(lf_head);
                    }
                }
                freed_chain_tail = Some(lf_tail);
            }

            // 5) If we fully drained the level, remove it from the half-book.
            if level_drained {
                match side {
                    Side::Bid => {
                        self.asks.levels.remove(&fill_price);
                        self.asks.price_index.remove(&fill_price);
                        if self.asks.best_price == Some(fill_price) {
                            self.asks.update_best_price();
                        }
                    }
                    Side::Ask => {
                        self.bids.levels.remove(&fill_price);
                        self.bids.price_index.remove(&fill_price);
                        if self.bids.best_price == Some(fill_price) {
                            self.bids.update_best_price();
                        }
                    }
                }
            }
        }

        // 6) Splice the sweep's freed chain into the slab freelist. One write
        //    to the chain tail's `next`, one update to `free_head`.
        if let Some(tail) = freed_chain_tail {
            let head = freed_chain_head.expect("invariant: head ⇒ tail");
            self.slab.slots[tail.as_usize()].next = self.slab.free_head;
            self.slab.free_head = Some(head);
        }

        Ok(remaining)
    }

    // (1) Match against the opposite side up to `price`
    // (2) Rest any unfilled remainder on our own side
    //     (a) Insert order in slab
    //     (b) Update price level and slab links
    //     (c) Return the engine-assigned `OrderHandle` to the caller
    pub fn add_limit_order(
        &mut self,
        side: Side,
        price: Price,
        quantity: u64,
    ) -> BookResult<Option<OrderHandle>> {
        // Limit semantics: stop matching when price no longer crosses.
        let remaining = self.match_against_opposite(side, Some(price), quantity)?;

        let handle = if remaining > 0 {
            let (order_idx, generation) = self.slab.alloc_slot()?;
            let slot = self.slab.get_mut(order_idx);
            slot.price = price;
            slot.quantity = remaining;
            slot.prev = None;
            slot.next = None;
            slot.side = side;

            match side {
                Side::Ask => self
                    .asks
                    .push_order_to_l2_book_and_update_slab_links(order_idx, &mut self.slab),
                Side::Bid => self
                    .bids
                    .push_order_to_l2_book_and_update_slab_links(order_idx, &mut self.slab),
            };

            Some(OrderHandle::new(order_idx, generation))
        } else {
            None
        };

        self.consumer.flush();
        Ok(handle)
    }

    // (1) Decode handle → (idx, generation); reject stale handles via the
    //     per-slot generation check (no hashmap probe)
    // (2) Read side + price from the slot (was kept in `order_index`)
    // (3) Unlink from price level (stitch prev/next, drop level if empty)
    // (4) Return slot to freelist (bumps generation, invalidating the handle)
    pub fn cancel_limit_order(&mut self, handle: OrderHandle) -> BookResult<()> {
        let idx = handle.idx();
        let generation = handle.generation();

        let slot = &self.slab.slots[idx.as_usize()];
        // Almost always hit; miss is the error path. Don't cmov.
        if slot.generation != generation {
            return Err(BookError::OrderNotFound);
        }
        let side = slot.side;
        let price = slot.price;

        match side {
            Side::Bid => self.bids.remove_order_from_l2_book_and_update_slab_links(
                idx,
                price,
                &mut self.slab,
            ),
            Side::Ask => self.asks.remove_order_from_l2_book_and_update_slab_links(
                idx,
                price,
                &mut self.slab,
            ),
        };

        self.slab.free(idx);

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
        let remaining = self.match_against_opposite(side, None, quantity)?;

        self.consumer.flush();
        Ok(MarketOrderResult { remaining })
    }
}

pub struct HalfBook {
    side: Side,
    levels: U64Map<Price, PriceLevel>,
    price_index: BTreeSet<Price>, // sorted; only walked on level drain
    best_price: Option<Price>,
}

impl HalfBook {
    fn new(side: Side) -> Self {
        Self {
            side,
            levels: U64Map::default(),
            price_index: BTreeSet::new(),
            best_price: None,
        }
    }

    /// (1) Update price level and stich slab links
    /// (2) Remove price level if empty (-price_index)
    /// (3) Refresh best price
    fn remove_order_from_l2_book_and_update_slab_links(
        &mut self,
        idx: SlabIndex,
        price: Price,
        slab: &mut OrderSlab,
    ) {
        if let Some(level) = self.levels.get_mut(&price) {
            level.remove(idx, slab);

            if level.is_empty() {
                self.levels.remove(&price);
                // Cold: only when a level dies.
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
    fn push_order_to_l2_book_and_update_slab_links(
        &mut self,
        idx: SlabIndex,
        slab: &mut OrderSlab,
    ) {
        let price = slab.get(idx).price;

        // Hash probe + possible alloc — not a 2-way select.
        let level = self.levels.entry(price).or_insert_with(|| {
            // Add new price set
            self.price_index.insert(price);

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

        // Head/mid/tail is predictable; both arms are one store. Don't cmov.
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
/// How the slab's backing memory was acquired. Determines what `Drop` does
/// (system free vs `munmap`). Hot-path code reads `slots[i]` and doesn't
/// touch this field.
#[derive(Clone, Copy)]
enum SlabBacking {
    System,
    #[cfg(target_os = "linux")]
    Mmap {
        bytes: usize,
    },
}

pub struct OrderSlab {
    slots: Vec<Order>,
    free_head: Option<SlabIndex>,
    capacity: usize,
    // Read only inside cfg(linux)'s Drop branch; on macOS the System variant
    // never needs inspection.
    #[allow(dead_code)]
    backing: SlabBacking,
}

impl OrderSlab {
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_in(capacity, SlabAllocator::System)
            .expect("SlabAllocator::System never fails")
    }

    /// Construct a slab with the requested allocator backing. On non-Linux
    /// platforms the Linux-only variants (`MadvHugepage`, `Hugetlb`) return
    /// `BookError::UnsupportedAllocator`.
    pub fn with_capacity_in(capacity: usize, alloc: SlabAllocator) -> BookResult<Self> {
        assert!(capacity > 0, "slab capacity must be non-zero");
        let total_slots = capacity + 1;

        // Acquire the backing storage. Each branch produces a `Vec<Order>`
        // pointing at memory of capacity `total_slots`. For mmap-backed
        // variants the Vec's allocator-ownership is *fake*: we must never
        // let the Vec free that memory (handled in `Drop`).
        let (mut slots, backing) = match alloc {
            SlabAllocator::System => (Vec::with_capacity(total_slots), SlabBacking::System),
            #[cfg(target_os = "linux")]
            SlabAllocator::MadvHugepage => unsafe {
                let bytes = total_slots
                    .checked_mul(std::mem::size_of::<Order>())
                    .ok_or(BookError::UnsupportedAllocator)?;
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    bytes,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if ptr == libc::MAP_FAILED {
                    return Err(BookError::UnsupportedAllocator);
                }
                if libc::madvise(ptr, bytes, libc::MADV_HUGEPAGE) != 0 {
                    libc::munmap(ptr, bytes);
                    return Err(BookError::UnsupportedAllocator);
                }
                // SAFETY: ptr came from mmap with `bytes` capacity in u8;
                // we re-interpret as `total_slots` Order entries. The Vec
                // is treated as a fixed-capacity buffer — we never call
                // any operation that would reallocate. Drop disarms it.
                let v = Vec::from_raw_parts(ptr as *mut Order, 0, total_slots);
                (v, SlabBacking::Mmap { bytes })
            },
            #[cfg(target_os = "linux")]
            SlabAllocator::Hugetlb => unsafe {
                let bytes = total_slots
                    .checked_mul(std::mem::size_of::<Order>())
                    .ok_or(BookError::UnsupportedAllocator)?;
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    bytes,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                    -1,
                    0,
                );
                if ptr == libc::MAP_FAILED {
                    return Err(BookError::UnsupportedAllocator);
                }
                let v = Vec::from_raw_parts(ptr as *mut Order, 0, total_slots);
                (v, SlabBacking::Mmap { bytes })
            },
            #[cfg(not(target_os = "linux"))]
            SlabAllocator::MadvHugepage | SlabAllocator::Hugetlb => {
                return Err(BookError::UnsupportedAllocator);
            }
        };

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
            side: Side::Bid,
            _pad: [0; 3],
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
                side: Side::Bid,
                _pad: [0; 3],
            });
        }

        Ok(Self {
            slots,
            free_head: Some(SlabIndex::new(1)),
            capacity,
            backing,
        })
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
    /// Wraps from `u32::MAX → 1`, skipping `0` to keep the `NonZeroU32`
    /// invariant.
    #[inline(always)]
    pub fn bump_generation(&mut self, idx: SlabIndex) {
        let slot = &mut self.slots[idx.as_usize()];
        let new_gen = slot.generation.get().wrapping_add(1);
        let new_gen = if new_gen == 0 { 1 } else { new_gen };
        // SAFETY: new_gen is non-zero by the branch above.
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

impl Drop for OrderSlab {
    fn drop(&mut self) {
        // System path: Vec's field destructor runs after this returns and
        // frees through the system allocator. Nothing to do here.
        #[cfg(target_os = "linux")]
        if let SlabBacking::Mmap { bytes } = self.backing {
            // SAFETY: ptr came from mmap with `bytes` length, exclusively
            // owned by this slab. We disarm Vec's destructor by overwriting
            // self.slots with an empty Vec — `ptr::write` doesn't drop the
            // old value, so the Vec pointing at mmap memory is never freed
            // through the system allocator (which would be UB). munmap
            // releases the mapping ourselves.
            unsafe {
                let ptr = self.slots.as_mut_ptr() as *mut libc::c_void;
                std::ptr::write(&mut self.slots, Vec::new());
                libc::munmap(ptr, bytes);
            }
        }
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
/// `next` (4) + `generation` (4) + `side` (1) + `_pad` (3). The
/// engine-assigned `OrderHandle` is `(generation, slab_index)` — the
/// index from the slot's position in the slab, the generation from the
/// field below — so no `order_id` field is needed on the slot. `side`
/// lives on the order because the slab is shared between bids and asks;
/// cancel reads it from the slot after the generation check.
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
    pub side: Side,
    _pad: [u8; 3],
}

const _: () = assert!(std::mem::size_of::<Order>() == 32);
const _: () = assert!(std::mem::align_of::<Order>() == 32);

/// Engine-assigned, opaque order handle.
///
/// Packed `(generation, slab_index)`: generation in the high 32 bits,
/// slab index in the low 32. Decoding is two shifts and a mask — no hash
/// lookup. The outer `NonZeroU64` lets `Option<OrderHandle>` niche-pack
/// to 8 bytes (the all-zero pattern becomes `None`).
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
/// 1. **Generation wraparound.** After ~4 billion frees of the *same*
///    slot, gen cycles through `1..=u32::MAX` and returns to a
///    previously-issued value. A handle sitting around for 2³² cycles
///    of its specific slot could match the slot's current gen again.
///    Practically unreachable — even at one free per nanosecond per
///    slot that's ~136 years per slot — but the type-level guarantee is
///    "u32 of generation space," not infinite.
///
/// 2. **Out-of-range `idx`.** If a caller manufactures or corrupts a
///    handle with `idx > slab.capacity`, the
///    `self.slab.slots[idx.as_usize()]` access in `cancel_limit_order`
///    panics *before* the generation check runs. Failure mode for
///    malformed handles is therefore "panic," not "OrderNotFound."
///    Fine for opaque, engine-issued handles; would need an explicit
///    bounds check if the API ever accepted untrusted handles (e.g.
///    deserialised over a network from another process).
///
/// 3. **Never-allocated slot.** All slots are initialised with
///    `gen = 1`. If a caller fabricates `OrderHandle { idx, gen: 1 }`
///    for an `idx` that has never been allocated, the gen check passes
///    (slot.gen and handle.gen both equal 1). `cancel_limit_order` then
///    reads garbage `side`/`price` from the slot, probably no-ops
///    against a non-existent level, then calls `free` — which bumps
///    gen and pushes the slot onto the freelist, **corrupting the
///    freelist by double-linking** (the slot was already there from
///    initialisation). Same root cause as (2): doesn't happen with
///    engine-issued handles, but the safety property is "the engine
///    controls all handles," not "any 8 bytes are safe to pass."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderHandle(NonZeroU64);

impl OrderHandle {
    #[inline(always)]
    pub fn new(idx: SlabIndex, generation: NonZeroU32) -> Self {
        let packed = ((generation.get() as u64) << 32) | (idx.0.get() as u64);
        // SAFETY: both halves are NonZero, so the packed value is non-zero.
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
        let hi = (self.0.get() >> 32) as u32;
        // SAFETY: constructed from a NonZeroU32 in the high half.
        unsafe { NonZeroU32::new_unchecked(hi) }
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

// Variant-agnostic surface. Thin delegation to the inherent methods; the
// associated `Handle` is this variant's own `OrderHandle` (no side bit here).
impl<C: FillConsumer + Default> crate::api::OrderBookApi for OrderBook<C> {
    type Handle = OrderHandle;

    #[inline]
    fn new(slab_capacity: usize) -> Self {
        OrderBook::new(slab_capacity)
    }

    fn new_with_alloc(slab_capacity: usize, alloc: SlabAllocator) -> BookResult<Self> {
        Ok(Self {
            bids: HalfBook::new(Side::Bid),
            asks: HalfBook::new(Side::Ask),
            slab: OrderSlab::with_capacity_in(slab_capacity, alloc)?,
            consumer: C::default(),
        })
    }

    #[inline]
    fn add_limit(
        &mut self,
        side: Side,
        price: Price,
        qty: u64,
    ) -> BookResult<Option<Self::Handle>> {
        self.add_limit_order(side, price, qty)
    }

    #[inline]
    fn add_market(
        &mut self,
        side: Side,
        qty: u64,
        mode: MarketOrderMode,
    ) -> BookResult<MarketOrderResult> {
        self.add_market_order(side, qty, mode)
    }

    #[inline]
    fn cancel(&mut self, handle: Self::Handle) -> BookResult<()> {
        self.cancel_limit_order(handle)
    }
}
