use anyhow::{Result, anyhow};
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
    #[inline(always)]
    fn match_against_opposite(
        &mut self,
        side: Side,
        price_limit: Option<Price>,
        quantity: u64,
    ) -> Result<u64> {
        let mut remaining = quantity;

        while remaining > 0 {
            // 1) Best price on the opposite side.
            let best_opposite = match side {
                Side::Bid => self.asks.best_price,
                Side::Ask => self.bids.best_price,
            };
            let fill_price = match best_opposite {
                Some(p) => p,
                None => break, // opposite side is empty
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

            // 3) Pull the head order of the best level on the opposite side.
            let level = match side {
                Side::Bid => self.asks.levels.get_mut(&fill_price),
                Side::Ask => self.bids.levels.get_mut(&fill_price),
            };
            let level = match level {
                Some(l) => l,
                None => break, // best_price desync — defensive break
            };
            let resting_idx = match level.head {
                Some(i) => i,
                None => break,
            };

            let order_ref = self.slab.get(resting_idx);
            let resting_qty = order_ref.quantity;
            let resting_id = order_ref.order_id;

            let fill_qty = remaining.min(resting_qty);
            remaining -= fill_qty;

            self.consumer.on_fill(Fill {
                resting_id,
                quantity: fill_qty,
            });

            // 4) Settle the resting order: full or partial.
            // (4.1) Inserts order in slab
            // (4.2) Updates price limit and links
            // (4.3) Updates order index map
            if fill_qty == resting_qty {
                let removed_idx = match side {
                    Side::Bid => self
                        .asks
                        .pop_order_from_l2_book_and_update_slab_links(fill_price, &mut self.slab),
                    Side::Ask => self
                        .bids
                        .pop_order_from_l2_book_and_update_slab_links(fill_price, &mut self.slab),
                }?;
                self.order_index.remove(&resting_id);
                self.slab.free(removed_idx);
            } else {
                self.slab.get_mut(resting_idx).quantity -= fill_qty;
                let level = match side {
                    Side::Bid => self.asks.levels.get_mut(&fill_price),
                    Side::Ask => self.bids.levels.get_mut(&fill_price),
                };
                if let Some(level) = level {
                    level.quantity -= fill_qty;
                }
            }
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
    ) -> Result<()> {
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

    // (3) Removes order index map
    // (2) Updates price limit and links
    // (1) Inserts order in slab
    pub fn cancel_limit_order(&mut self, order_id: OrderId) -> Result<()> {
        let (idx, side, price) = self
            .order_index
            .remove(&order_id)
            .ok_or_else(|| anyhow!("Order not found"))?;

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
        }

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
    ) -> Result<MarketOrderResult> {
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

    /// (1) Update price level and stich slab links
    /// (2) Remove price level if empty (-price_index)
    /// (3) Refresh best price
    fn pop_order_from_l2_book_and_update_slab_links(
        &mut self,
        price: Price,
        slab: &mut OrderSlab,
    ) -> Result<SlabIndex> {
        if let Some(level) = self.levels.get_mut(&price) {
            let removed_idx = level
                .pop_order_from_l2_level_and_update_slab_links(slab)
                .ok_or_else(|| anyhow!("No order to pop?"))?;

            if level.is_empty() {
                self.levels.remove(&price);
                self.price_index.remove(&price);

                // Refresh the best price if needed
                if self.best_price == Some(price) {
                    self.update_best_price();
                };
            }

            Ok(removed_idx)
        } else {
            return Err(anyhow!("No price level found, whaaat"));
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

    /// Remove the front order from the queue. O(1).
    /// Returns the SlabIndex of the removed order.
    pub fn pop_order_from_l2_level_and_update_slab_links(
        &mut self,
        slab: &mut OrderSlab,
    ) -> Option<SlabIndex> {
        let head_idx = self.head?;
        let next = slab.get(head_idx).next;

        if let Some(next_idx) = next {
            // We informe the next order slot that the previous
            // order slot is now free
            slab.get_mut(next_idx).prev = None;
        } else {
            // If there is no next slot then it means the order queue is empty
            self.tail = None;
        };
        self.head = next;

        self.quantity -= slab.get(head_idx).quantity;
        self.order_count -= 1;

        Some(head_idx)
    }
}

/// The slab allocator.
pub struct OrderSlab {
    slots: Vec<OrderSlot>,
    free_head: Option<u32>, // head of the free list
    capacity: usize,
}

impl OrderSlab {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "slab capacity must be non-zero");

        let mut slots = Vec::with_capacity(capacity);

        for i in 0..capacity {
            let next_free = if i + 1 < capacity {
                Some((i + 1) as u32)
            } else {
                None
            };
            slots.push(OrderSlot::Free { next_free })
        }

        Self {
            slots,
            free_head: Some(0),
            capacity,
        }
    }

    pub fn get(&self, idx: SlabIndex) -> &Order {
        match &self.slots[idx.0 as usize] {
            OrderSlot::Occupied(order) => order,
            OrderSlot::Free { .. } => panic!("get() on free slot {:?}", idx),
        }
    }

    pub fn get_mut(&mut self, idx: SlabIndex) -> &mut Order {
        match &mut self.slots[idx.0 as usize] {
            OrderSlot::Occupied(order) => order,
            OrderSlot::Free { .. } => panic!("get_mut() on free slot {:?}", idx),
        }
    }

    pub fn free(&mut self, idx: SlabIndex) {
        let old_head = self.free_head;
        self.slots[idx.0 as usize] = OrderSlot::Free {
            next_free: old_head,
        };
        self.free_head = Some(idx.0);
    }

    pub fn insert_order(&mut self, order: Order) -> Result<SlabIndex> {
        if let Some(idx) = self.free_head {
            let slot = &self.slots[idx as usize];

            // This is the type of error catching we want to find in exhaustive testing
            // but eliminate it out of prod code
            self.free_head = match slot {
                OrderSlot::Free { next_free } => *next_free,
                OrderSlot::Occupied(_) => unreachable!("free list points to occupied slot"),
            };

            self.slots[idx as usize] = OrderSlot::Occupied(order);
            Ok(SlabIndex(idx))
        } else {
            return Err(anyhow!("Slab is full"));
        }
    }
}

/// A slot in the slab — either occupied or free (part of free list).
#[derive(Debug)]
enum OrderSlot {
    Occupied(Order),
    Free { next_free: Option<u32> }, // index of next free slot
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
