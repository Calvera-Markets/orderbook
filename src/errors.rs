/// Thin, `Copy` error type for the hot path.
///
/// `anyhow::Error` is a fat pointer to a heap-allocated `Box<dyn Error + Send + Sync>`
/// — every `Err(anyhow!(...))` is at least one allocation, plus a `String` format if
/// the message is non-static. That cost is irrelevant on cold setup paths but it is
/// a real tax on the matching loop, which calls
/// `pop_order_from_l2_book_and_update_slab_links` once per fill. Using a 1-byte enum
/// keeps `Result<T, BookError>` stack-only and lets the compiler niche-pack it next
/// to small `T`s.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BookError {
    /// `cancel_limit_order` was called with an id that isn't in `order_index`.
    OrderNotFound,
    /// `OrderSlab::insert_order` was called with no free slots left.
    SlabFull,
    /// A `PriceLevel` was found but its FIFO queue was empty — invariant violation.
    EmptyLevel,
    /// `best_price` pointed at a price with no matching entry in `levels` — invariant violation.
    MissingLevel,
}

pub type BookResult<T> = core::result::Result<T, BookError>;
