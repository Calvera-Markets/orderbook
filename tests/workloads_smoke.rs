//! Test runner for the `workloads.rs` module. The actual test cases live
//! inside `workloads.rs` under `#[cfg(test)] mod tests` so they sit next to
//! the code they exercise; this file is just the integration-test entry point
//! that `cargo test` auto-discovers.

#[path = "../workloads.rs"]
mod workloads;
