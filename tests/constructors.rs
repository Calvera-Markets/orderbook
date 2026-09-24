//! Public constructors and encodings the parity scenarios never touch.

use calvera_books::api::OrderBookApi;
use calvera_books::types::{Price, Side, SlabAllocator};
use calvera_books::{BookError, OrderBook, OrderSlab, VecConsumer};

#[test]
fn slab_allocator_slugs_round_trip() {
    let cases = [
        (SlabAllocator::System, "system", &["system", "vec"][..]),
        (
            SlabAllocator::MadvHugepage,
            "madvise",
            &["madvise", "madv_hugepage", "madv-hugepage"][..],
        ),
        (
            SlabAllocator::Hugetlb,
            "hugetlb",
            &["hugetlb", "map_hugetlb", "map-hugetlb"][..],
        ),
    ];
    for (alloc, slug, aliases) in cases {
        assert_eq!(alloc.slug(), slug);
        for alias in aliases {
            assert_eq!(SlabAllocator::parse(alias), Some(alloc));
        }
    }
    assert_eq!(
        SlabAllocator::parse("  system  "),
        Some(SlabAllocator::System)
    );
    assert_eq!(SlabAllocator::parse("nope"), None);
}

#[test]
fn asymmetric_capacities_fill_the_smaller_side_first() {
    let mut book = OrderBook::<VecConsumer>::with_capacities(1, 4);

    let bid = book
        .add_limit_order(Side::Bid, Price(100), 1)
        .unwrap()
        .expect("bid rests");
    assert_eq!(
        book.add_limit_order(Side::Bid, Price(99), 1),
        Err(BookError::SlabFull)
    );

    let ask = book
        .add_limit_order(Side::Ask, Price(101), 1)
        .unwrap()
        .expect("ask rests");
    assert_ne!(ask.as_u64(), bid.as_u64());
    book.cancel_limit_order(ask).unwrap();
    book.cancel_limit_order(bid).unwrap();
}

#[test]
fn hugepage_allocators_are_unsupported_off_linux() {
    let madv = OrderBook::<VecConsumer>::new_with_alloc(8, SlabAllocator::MadvHugepage);
    let huge = OrderBook::<VecConsumer>::new_with_alloc(8, SlabAllocator::Hugetlb);
    #[cfg(not(target_os = "linux"))]
    {
        assert!(matches!(madv, Err(BookError::UnsupportedAllocator)));
        assert!(matches!(huge, Err(BookError::UnsupportedAllocator)));
    }
    #[cfg(target_os = "linux")]
    {
        // mmap may succeed or fail closed, depending on the hugepage pool.
        let _ = (madv, huge);
    }
}

#[test]
fn handle_as_u64_keeps_the_side_bit() {
    let mut book = OrderBook::<VecConsumer>::new(16);
    let bid = book
        .add_limit_order(Side::Bid, Price(100), 1)
        .unwrap()
        .expect("bid rests");
    let ask = book
        .add_limit_order(Side::Ask, Price(101), 1)
        .unwrap()
        .expect("ask rests");

    assert_eq!(bid.as_u64() >> 63, 0);
    assert_eq!(ask.as_u64() >> 63, 1);
    assert_eq!(bid.side(), Side::Bid);
    assert_eq!(ask.side(), Side::Ask);
}

#[test]
fn slab_reports_the_capacity_it_was_built_with() {
    let mut slab = OrderSlab::with_capacity(2);
    assert_eq!(slab.capacity(), 2);
    assert!(slab.alloc_slot().is_ok());
    assert!(slab.alloc_slot().is_ok());
    assert_eq!(slab.alloc_slot(), Err(BookError::SlabFull));
}
