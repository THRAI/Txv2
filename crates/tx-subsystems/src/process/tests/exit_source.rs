// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

// Tests for the per-process `exit_source` wait source.

use super::*;
use crate::process::structure::EXIT_SOURCE_CHILD_ZOMBIFIED;

#[test]
fn process_payload_exit_source_default_constructed_and_registered() {
    let _g = setup();
    let init = bootstrap();

    let carrier_id = init
        .exit_source_id()
        .expect("live init has an exit_source carrier id");
    // Must be a non-zero, registered id.
    assert!(carrier_id != 0);
    assert!(crate::wait_source::lookup_wait_channel(carrier_id).is_some());

    // Exit_port wait token is built from the carrier id + the
    // child-zombified bit.
    let token = init
        .exit_source_wait_token()
        .expect("live init has an exit_source wait token");
    assert_eq!(token.source_id(), carrier_id);
    assert_eq!(token.interest(), EXIT_SOURCE_CHILD_ZOMBIFIED);
}

#[test]
fn post_sigchld_to_parent_fires_exit_source() {
    use crate::process::adapter::wait_routing::Mask;
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");

    // Park a wait future on the parent's exit_source BEFORE the
    // child zombifies so the fire site has an awaiter to release.
    let token = parent
        .exit_source_wait_token()
        .expect("live parent has token");
    let channel = crate::wait_source::lookup_wait_channel(token.source_id())
        .expect("registered carrier resolves");
    let mut wait = channel.wait(Mask::from_bits(token.interest()));

    // Drive a single poll to register the awaiter.
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: vtable functions are no-ops.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let pre = Pin::new(&mut wait).poll(&mut cx);
    assert!(matches!(pre, Poll::Pending), "no fires yet → Pending");

    // Zombify the child via step_exit_group; this calls
    // post_sigchld_to_parent which fires the parent's exit_source.
    step_exit_group(&child, ExitStatus::Exited(0));

    // The awaiter should now resolve on next poll.
    let post = Pin::new(&mut wait).poll(&mut cx);
    assert!(
        matches!(post, Poll::Ready(_)),
        "exit_source fire on zombification should release the parked awaiter",
    );
}

#[test]
fn step_fork_clones_init_with_fresh_exit_source() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false).expect("fork");

    let parent_id = parent.exit_source_id().expect("parent live");
    let child_id = child.exit_source_id().expect("child live");
    assert_ne!(
        parent_id, child_id,
        "each ProcessPayload gets its own Channel + carrier id",
    );

    // Both carriers must resolve through the registry.
    assert!(crate::wait_source::lookup_wait_channel(parent_id).is_some());
    assert!(crate::wait_source::lookup_wait_channel(child_id).is_some());
}

#[test]
fn zombie_process_exit_source_id_is_none() {
    let _g = setup();
    let init = bootstrap();
    step_exit_group(&init, ExitStatus::Exited(0));
    assert!(init.is_zombie());

    // After zombification the payload is gone; the carrier id is
    // unobservable through the identity accessor (the underlying
    // registry slot may still hold the channel — see
    // Cross-cutting Risk #1 in the slice plan; that's a Wave 2+
    // cleanup item).
    assert!(init.exit_source_id().is_none());
    assert!(init.exit_source_wait_token().is_none());
    // fire_exit_source on a zombie is a no-op (returns 0).
    assert_eq!(
        init.fire_exit_source(crate::process::adapter::wait_routing::Mask::from_bits(
            EXIT_SOURCE_CHILD_ZOMBIFIED
        )),
        0,
    );
}
