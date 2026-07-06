//! Genuinely encoding-agnostic value types shared across all `OrderBook`
//! variants. No design opinion lives here.
//!
//! Deliberately excluded: `OrderHandle` (its bit layout is variant-specific —
//! v1 has no side bit, v2 packs the side), `SlabIndex`, and `Fill`. Those stay
//! in each variant module; the framework abstracts the handle through the
//! `OrderBookApi::Handle` associated type rather than a shared concrete type.

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

/// Backing allocator for the engine's slab.
///
/// The slab is the dominant single allocation in the engine (~32 B × capacity).
/// On Linux, page-fault count for fresh slabs is proportional to capacity
/// divided by page size — 4× more faults on 4 KB pages than 16 KB. Routing
/// the slab buffer through huge-page-friendly allocators (madvise/hugetlb)
/// cuts that to ~zero. See `lessons/pages.md`.
///
/// Non-default variants are Linux-only; constructing an `OrderBook` with
/// them on other platforms returns `BookError::UnsupportedAllocator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlabAllocator {
    /// Default. Slab backed by `Vec<Order>` via the system allocator.
    /// Cross-platform; the only choice that works on macOS.
    System,
    /// Linux: `mmap` + `madvise(MADV_HUGEPAGE)` — kernel *prefers* (but
    /// doesn't guarantee) 2 MB pages for the slab. Reduces TLB pressure
    /// and page-fault count without needing a pre-reserved hugepage pool.
    MadvHugepage,
    /// Linux: `mmap(MAP_HUGETLB)` — forces 2 MB pages from a pre-reserved
    /// hugepage pool. Requires admin to populate `/proc/sys/vm/nr_hugepages`.
    /// More aggressive than `MadvHugepage`; fails fast if pool is empty.
    Hugetlb,
}

impl SlabAllocator {
    /// Short slug used in bench ids on disk.
    pub fn slug(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::MadvHugepage => "madvise",
            Self::Hugetlb => "hugetlb",
        }
    }

    /// Parse from an env-var-friendly form. Accepts canonical slugs plus
    /// common aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "system" | "vec" => Some(Self::System),
            "madvise" | "madv_hugepage" | "madv-hugepage" => Some(Self::MadvHugepage),
            "hugetlb" | "map_hugetlb" | "map-hugetlb" => Some(Self::Hugetlb),
            _ => None,
        }
    }
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
