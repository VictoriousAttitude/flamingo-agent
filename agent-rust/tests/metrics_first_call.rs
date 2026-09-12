//! Regression test for the memory-stats first-call race: `memory-stats` initialises its Linux
//! statics on first use without ordering them, so a concurrent first call can read a
//! half-initialised state and report zero.
//!
//! This lives in its own integration test binary (rather than `metrics.rs`'s test module) so
//! that `WARM_UP` is guaranteed unfired when the race is exercised: every test file in
//! `agent-rust/tests/` compiles to a separate binary with its own process and its own copy of
//! the crate's statics, so no other test can have already warmed it up.

use flamingo_agent::metrics::rss_bytes;

#[test]
fn first_call_is_serialized_across_threads() {
    let handles: Vec<_> = (0..16).map(|_| std::thread::spawn(rss_bytes)).collect();

    for handle in handles {
        let result = handle.join().unwrap();
        assert!(matches!(result, Ok(n) if n > 0));
    }
}
