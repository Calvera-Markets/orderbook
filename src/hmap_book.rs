/// Optimizations:
/// (1) Instead of using VECs:
/// - A simple array can't be used because it would blow the stack
///
/// - mmap with MAP_HUGETLB. Instead of Vec or a static array, you allocate directly
/// from the OS using huge pages (2MB or 1GB pages instead of 4KB). This keeps the entire
/// slab in a small number of TLB entries, dramatically reducing TLB pressure when you're
/// scanning lots of orders. The allocation is still heap-like (it's a pointer), but the memory
/// is physically contiguous and huge-page-backed:
/// rust// pseudocode — real impl `uses libc::mmap`
/// ```rust
/// let ptr = mmap(size, MAP_ANONYMOUS | MAP_HUGETLB);
/// let slab: &mut [OrderSlot] = slice::from_raw_parts_mut(ptr, capacity
/// ```
///
/// - LMAX approach: declare the entire engine state as a single large struct, allocate it once with a custom allocator pinned to a NUMA node, and never move it. The "slab" is just a field in that struct. In Rust this looks like using #[repr(C)] with explicit field ordering to control cache line layout.
///
/// The pointer indirection of Vec costs maybe 1-2 cycles on a cache-warm access. What actually kills you at HFT latency is:
///
/// Cache misses when the order you're accessing is cold (not recently touched)
/// TLB misses when your slab spans thousands of 4KB pages
/// False sharing when two CPU cores write to slots in the same cache line
/// None of those are solved by switching from Vec to an array. They're solved by huge pages, NUMA-aware allocation, and cache-line padding between slots that different threads touch.
///
/// (2) Branchless programming, instead of pattern matching on trade side, maybe we can just
/// rely on arithmetic
///
/// (3) There is some degree of cross-referentiality in our data structures. Instead of using indices
/// we should use pointers.
use std::collections::{BTreeSet, HashMap};

use crate::errors::{BookError, BookResult};

pub struct OrderBook<C: FillConsumer> {
    bids: HalfBook,
    asks: HalfBook,
    slab: OrderSlab,
    /// order_id → (SlabIndex, Side, Price)
    /// Required for O(1) cancel without scanning the book.
    order_index: HashMap<OrderId, (SlabIndex, Side, Price)>,
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
            order_index: HashMap::with_capacity(slab_capacity),
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
        let mut remaining = quantity;
        let mut freed_chain_head: Option<SlabIndex> = None;
        let mut freed_chain_tail: Option<SlabIndex> = None;

        'sweep: while remaining > 0 {
            // 1) Best price on the opposite side.
            let best_opposite = match side {
                Side::Bid => self.asks.best_price,
                Side::Ask => self.bids.best_price,
            };
            let fill_price = match best_opposite {
                Some(p) => p,
                None => break,
            };

            // 2) Limit-order stop condition: bail if our price no longer crosses.
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
                    let slot = &self.slab.slots[current_idx.0 as usize];
                    let resting_qty = slot.quantity;
                    let resting_id = slot.order_id;
                    let next_idx = slot.next;

                    let fill_qty = remaining.min(resting_qty);
                    remaining -= fill_qty;
                    self.consumer.on_fill(Fill {
                        resting_id,
                        quantity: fill_qty,
                    });

                    if fill_qty == resting_qty {
                        // Full consume. Donate to this level's freed chain
                        // (no slab write — already linked via `Order.next`).
                        self.order_index.remove(&resting_id);
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
                                    self.slab.slots[n.0 as usize].prev = None;
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
                        let s = &mut self.slab.slots[current_idx.0 as usize];
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
                        self.slab.slots[prev_tail.0 as usize].next = Some(lf_head);
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
            self.slab.slots[tail.0 as usize].next = self.slab.free_head;
            self.slab.free_head = Some(head);
        }

        Ok(remaining)
    }

    // (1) Match against the opposite side up to `price`
    // (2) Rest any unfilled remainder on our own side
    //     (a) Insert order in slab
    //     (b) Update price level and slab links
    //     (c) Update order index map
    pub fn add_limit_order(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: Price,
        quantity: u64,
    ) -> BookResult<()> {
        // Limit semantics: stop matching when price no longer crosses.
        let remaining = self.match_against_opposite(side, Some(price), quantity)?;

        if remaining > 0 {
            let new_order = Order {
                order_id,
                price,
                quantity: remaining,
                side,
                prev: None,
                next: None,
            };

            let order_idx = self.slab.insert_order(new_order)?;

            match side {
                Side::Ask => self
                    .asks
                    .push_order_to_l2_book_and_update_slab_links(order_idx, &mut self.slab),
                Side::Bid => self
                    .bids
                    .push_order_to_l2_book_and_update_slab_links(order_idx, &mut self.slab),
            };

            self.order_index.insert(order_id, (order_idx, side, price));
        }

        self.consumer.flush();
        Ok(())
    }

    // (1) Lookup + remove from order_index
    // (2) Unlink from price level (stitch prev/next, drop level if empty)
    // (3) Return slot to freelist
    pub fn cancel_limit_order(&mut self, order_id: OrderId) -> BookResult<()> {
        let (idx, side, price) = self
            .order_index
            .remove(&order_id)
            .ok_or(BookError::OrderNotFound)?;

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
        _order_id: OrderId,
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
                    filled_quantity: 0,
                    unfilled_qty: quantity,
                    cancelled: true,
                });
            }
        }

        // Market = sweep unconditionally → price_limit = None.
        let remaining = self.match_against_opposite(side, None, quantity)?;
        let filled_quantity = quantity - remaining;
        let cancelled = remaining > 0;

        self.consumer.flush();
        Ok(MarketOrderResult {
            filled_quantity,
            unfilled_qty: remaining,
            cancelled,
        })
    }
}

pub struct HalfBook {
    side: Side,
    levels: HashMap<Price, PriceLevel>,
    price_index: BTreeSet<Price>, // sorted; only walked on level drain
    best_price: Option<Price>,
}

impl HalfBook {
    fn new(side: Side) -> Self {
        Self {
            side,
            levels: HashMap::new(),
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

        // Get or insert level
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

/// Fixed-point price. 1 unit = 1 tick of the instrument.
/// All price arithmetic is integer arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

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

/// Slab allocator with no per-slot discriminant.
///
/// Slots store `Order` directly. When a slot is on the freelist, its
/// `Order.next` field is repurposed as the freelist link; the other fields
/// (`order_id`, `price`, `quantity`, `side`, `prev`) are garbage. Callers
/// must not read a free slot — invariant maintained by the matcher and by
/// `order_index` only holding indices of occupied slots.
pub struct OrderSlab {
    slots: Vec<Order>,
    free_head: Option<SlabIndex>,
    capacity: usize,
}

impl OrderSlab {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "slab capacity must be non-zero");

        let mut slots = Vec::with_capacity(capacity);
        for i in 0..capacity {
            let next_free = if i + 1 < capacity {
                Some(SlabIndex((i + 1) as u32))
            } else {
                None
            };
            // Placeholder `Order` — only `next` (the freelist link) is
            // meaningful while the slot is free.
            // TODO: init through Default trait, but we need to remove the side first
            slots.push(Order {
                order_id: OrderId(0),
                price: Price(0),
                quantity: 0,
                side: Side::Bid,
                prev: None,
                next: next_free,
            });
        }

        Self {
            slots,
            free_head: Some(SlabIndex(0)),
            capacity,
        }
    }

    #[inline(always)]
    pub fn get(&self, idx: SlabIndex) -> &Order {
        &self.slots[idx.0 as usize]
    }

    #[inline(always)]
    pub fn get_mut(&mut self, idx: SlabIndex) -> &mut Order {
        &mut self.slots[idx.0 as usize]
    }

    pub fn free(&mut self, idx: SlabIndex) {
        self.slots[idx.0 as usize].next = self.free_head;
        self.free_head = Some(idx);
    }

    pub fn insert_order(&mut self, order: Order) -> BookResult<SlabIndex> {
        let idx = self.free_head.ok_or(BookError::SlabFull)?;
        // The slot's `next` field holds the freelist link while free; read it
        // before we overwrite the slot with the caller's `Order`.
        let next_free = self.slots[idx.0 as usize].next;
        self.free_head = next_free;
        self.slots[idx.0 as usize] = order;
        Ok(idx)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// A resting limit order.
#[derive(Debug, Clone)]
pub struct Order {
    pub order_id: OrderId,
    pub price: Price,
    pub quantity: u64, // remaining quantity (decremented on partial fill)
    pub side: Side,

    // Intrusive linked list pointers within a price level's FIFO queue.
    // None = this is the head (prev) or tail (next) of the queue.
    // These are slab indices, not raw pointers — safe to store in a Vec.
    pub prev: Option<SlabIndex>, // order ahead in queue (older, higher priority)
    pub next: Option<SlabIndex>, // order behind in queue (newer, lower priority)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

/// Index into the slab — this is what we store instead of Box<Order>.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlabIndex(pub u32);

#[derive(Debug)]
pub struct MarketOrderResult {
    pub filled_quantity: u64,
    pub unfilled_qty: u64, // > 0 means partial fill (IOC) or full cancel (FOK)
    pub cancelled: bool,   // true if any quantity was cancelled
}

#[derive(Debug)]
pub struct Fill {
    pub resting_id: OrderId,
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
