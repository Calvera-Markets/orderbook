pub mod errors;
pub mod hmap_book;
pub mod hmap_book_v2;

pub use errors::*;
// No glob re-exports of the two book impls — they share type names, so the
// ambiguity surfaces as an error at external use sites. Benches and other
// callers pick a version explicitly via `hmap_book::*` or `hmap_book_v2::*`.
