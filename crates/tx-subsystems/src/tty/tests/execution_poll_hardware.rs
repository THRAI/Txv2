//! TTY execution hardware polling step tests.

use crate::tty::adapter::step_engine::{
    guard, ByteProgress, Errno as V3Errno, StepOutcome, StepOutcome as V3Out, YieldShape,
};
use alloc::boxed::Box;

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::Guard;
use crate::tty::execution::{register_hardware, step_poll_hardware_input};
use crate::tty::structure::{TtyKind, TtyPayload};

use super::support::{alloc_tty, init_zones, NOOP_BINDING, TTY_ZONE_TEST_LOCK};

struct BlockingReadOps;

impl CharDeviceOps for BlockingReadOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> V3Out<usize, ByteProgress> {
        V3Out::yield_on_wait_source(ByteProgress::EMPTY, 0x55, 0x0f)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> V3Out<usize, ByteProgress> {
        V3Out::Done(bytes.len())
    }
}

static BLOCKING_READ_OPS: BlockingReadOps = BlockingReadOps;

#[test]
fn step_poll_hardware_input_rejects_pty_transport() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_zones();
    let guard = guard();

    let slave = alloc_tty(
        TtyKind::PtySlave,
        2,
        "pts/2",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let master = alloc_tty(
        TtyKind::PtyMaster,
        2,
        "ptmx2",
        TtyPayload::new_pty_master(slave),
    );

    assert_eq!(
        step_poll_hardware_input(&master, 16, &guard),
        V3Out::Err(V3Errno::EINVAL)
    );
}

#[test]
fn step_poll_hardware_input_propagates_blocked_driver_read() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_zones();
    let guard = guard();

    let binding = Box::leak(Box::new(CharDeviceBinding {
        devt: DevT::new(4, 66),
        name: "ttySblocked",
        ops: &BLOCKING_READ_OPS,
    }));
    let tty = match register_hardware("ttySblocked", 2, binding, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };

    match step_poll_hardware_input(&tty, 16, &guard) {
        V3Out::Yield {
            shape:
                YieldShape::OnWaitSource {
                    source: carrier,
                    interests,
                },
            ..
        } => {
            assert_eq!(carrier.raw(), 0x55);
            assert_eq!(interests.raw(), 0x0f);
        }
        other => panic!("expected v3 Yield::OnWaitSource, got {other:?}"),
    }
}
