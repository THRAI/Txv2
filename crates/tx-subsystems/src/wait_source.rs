//! `WaitToken` → `Channel` resolver.
//!
//! Subsystems that can produce `Blocked(WaitToken)` outcomes register their
//! wake `Channel` here and embed the returned carrier id in any
//! `WaitToken` they hand out. Async script wrappers convert a `WaitToken`
//! into an awaitable `WaitFuture` via `wait_on_token`.
//!
//! Test-only `WaitToken` placeholders constructed with arbitrary
//! `WaitToken::new(carrier, interest)` literals do not register anything,
//! so `wait_on_token` returns `None` for them. Production code is expected
//! to construct `WaitToken` values whose carrier is a registered id.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};
use tx_reactor::wait::{Channel, Mask, WaitFuture};

use tx_substrate::SpinMutex;

use crate::execution::WaitToken;

static REGISTRY: SpinMutex<BTreeMap<u64, Channel>> = SpinMutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register `channel` for carrier-based wait resolution. Returns the
/// carrier id that consumers should embed in their `WaitToken` values. The
/// registry holds an internal clone; the caller's channel handle remains
/// independent. Carrier ids are non-zero and monotonically increasing.
pub fn register_wait_channel(channel: Channel) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
    REGISTRY.lock().insert(id, channel);
    id
}

/// Drop the registry's clone of the channel registered under `id`.
/// Subsequent `lookup_wait_channel` and `wait_on_token` calls for `id`
/// return `None`. Idempotent: releasing an unregistered id is a no-op.
pub fn release_wait_channel(id: u64) {
    REGISTRY.lock().remove(&id);
}

/// Return a clone of the channel registered under `id`, or `None` if no
/// channel is currently registered for that id.
pub fn lookup_wait_channel(id: u64) -> Option<Channel> {
    REGISTRY.lock().get(&id).cloned()
}

/// Convert `token` into a `WaitFuture` over the registered channel. Returns
/// `None` if `token.source_id()` is not a currently-registered id (typical
/// for test placeholder tokens constructed via raw `WaitToken::new`).
pub fn wait_on_token(token: WaitToken) -> Option<WaitFuture> {
    let channel = lookup_wait_channel(token.source_id())?;
    Some(channel.wait(Mask::from_bits(token.interest())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_source_register_returns_nonzero_distinct_ids() {
        let a = register_wait_channel(Channel::new());
        let b = register_wait_channel(Channel::new());
        assert!(a != 0);
        assert!(b != 0);
        assert_ne!(a, b);
        release_wait_channel(a);
        release_wait_channel(b);
    }

    #[test]
    fn wait_source_lookup_returns_some_for_registered_id_and_none_after_release() {
        let id = register_wait_channel(Channel::new());
        assert!(lookup_wait_channel(id).is_some());
        release_wait_channel(id);
        assert!(lookup_wait_channel(id).is_none());
    }

    #[test]
    fn wait_source_lookup_returns_none_for_unregistered_id() {
        assert!(lookup_wait_channel(0xdead_beef_dead_beef).is_none());
    }

    #[test]
    fn wait_on_token_returns_some_for_registered_carrier() {
        let id = register_wait_channel(Channel::new());
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_some());
        release_wait_channel(id);
    }

    #[test]
    fn wait_on_token_returns_none_for_test_placeholder_token() {
        // Existing test mocks (BlockingFs, LifecycleFs) construct tokens with
        // arbitrary numbers; those tokens must not panic in
        // wait_on_token; they return None so production await sites can
        // treat them as a sentinel.
        //
        // Use an obviously-out-of-range ID — Slice 3 (futex) registers
        // 256 carriers at zone-init time, claiming IDs 1..N for some
        // N that grows with each new wait-carrier producer. A
        // sentinel above the entire u32 space is never collisional.
        let token = WaitToken::new(u64::MAX - 1, 0x55);
        assert!(wait_on_token(token).is_none());
    }

    #[test]
    fn wait_on_token_returns_none_after_release() {
        let id = register_wait_channel(Channel::new());
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_some());
        release_wait_channel(id);
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_none());
    }
}
