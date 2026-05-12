//! Shared TTY test fixtures.

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::Guard;
use crate::tty::adapter::step_engine::{
    reserve_for, sign_for, ByteProgress, Cap, PayloadCap, StepOutcome as V3,
};
use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};

pub(super) use crate::test_support::EPOCH_TEST_LOCK as TTY_ZONE_TEST_LOCK;

struct NoopOps;

impl CharDeviceOps for NoopOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> V3<usize, ByteProgress> {
        V3::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> V3<usize, ByteProgress> {
        V3::Done(bytes.len())
    }
}

static NOOP_OPS: NoopOps = NoopOps;

pub(super) static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "tty-test",
    ops: &NOOP_OPS,
};

pub(super) fn init_zones() {
    tx_substrate::testing::init_host_for_test_once();
    crate::tty::structure::registry::register_zones().expect("tty zones");
    crate::tty::structure::registry::reset_for_tests();
}

pub(super) fn alloc_tty(
    kind: TtyKind,
    index: u32,
    name: &str,
    payload: TtyPayload,
) -> Cap<TtyIdentity> {
    let id_res = reserve_for::<TtyIdentity>().expect("tty identity reservation");
    let payload_res = reserve_for::<TtyPayload>().expect("tty payload reservation");
    let payload = PayloadCap::from_cap(sign_for(payload_res, payload));
    let identity = sign_for(id_res, TtyIdentity::new(kind, index, name));
    identity.install_payload(payload);
    identity
}
