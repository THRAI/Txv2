//! P-agnostic monotonic clock bridge for the network stack.
//!
//! The step/socket layer (`protocol/tcp.rs` `with_context`) hands smoltcp
//! its `Context` from code that has no `P: TimeIf` generic, so it cannot
//! read the HAL clock directly. Layers that do have `P` — the net delegate
//! step and the socket syscall entries — publish the real monotonic time
//! here, and the P-agnostic layer reads it back. Defaults to 0, which
//! reproduces the pre-bridge frozen-clock behavior for any path that never
//! publishes a time. See `docs/design/07_net/REFACTOR_P0_v1.md` and
//! `REFACTOR_PLAN_A_v2.md` §3 D3.

use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::time::Instant;

/// Real monotonic time in nanoseconds, published by clock-having layers.
pub static NET_NOW_NS: AtomicU64 = AtomicU64::new(0);

/// Publish the current monotonic time in nanoseconds.
pub fn net_set_now_ns(ns: u64) {
    // The source clock is monotonic and this is only a freshness hint;
    // Relaxed is enough.
    NET_NOW_NS.store(ns, Ordering::Relaxed);
}

/// Read the bridge as a smoltcp `Instant` (microsecond granularity).
pub fn net_now_instant() -> Instant {
    Instant::from_micros((NET_NOW_NS.load(Ordering::Relaxed) / 1_000) as i64)
}
