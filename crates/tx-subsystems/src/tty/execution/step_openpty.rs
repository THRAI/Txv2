//! Minimal pty pair creation.

use crate::tty::adapter::step_engine::{self as step_engine, Cap, PayloadCap};

use crate::execution::{Errno, Guard};
use crate::tty::adapter::step_engine::{
    NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
use crate::tty::project;
use crate::tty::structure::registry;
use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
use crate::vfs::OpenFile;

// Re-import v3 types via local alias for brevity in fn body.

#[derive(Debug)]
pub struct OpenPtyOutcome {
    pub index: u32,
    pub master: Cap<TtyIdentity>,
    pub slave: Cap<TtyIdentity>,
    pub master_file: Cap<OpenFile>,
    pub slave_file: Cap<OpenFile>,
}

/// Create master/slave TTY identities, install peer-linked payloads, publish
/// the slave into the devpts registry, and return OpenFiles for both sides.
pub fn step_openpty(guard: &Guard<'_>) -> StepOutcome<OpenPtyOutcome, NoProgress> {
    use crate::tty::adapter::step_engine::StepOutcome as V3;

    let index = match registry::allocate_pty_index() {
        Ok(index) => index,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    if registry::contains_pty_slave(index) {
        return V3::Err(Errno::EIO.into());
    }

    let master_id_res = match step_engine::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let slave_id_res = match step_engine::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let master_payload_res = match step_engine::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let slave_payload_res = match step_engine::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };

    let master = step_engine::sign_for(
        master_id_res,
        TtyIdentity::new(TtyKind::PtyMaster, index, "ptmx"),
    );
    let slave_name = PtsName::new(index);
    let slave = step_engine::sign_for(
        slave_id_res,
        TtyIdentity::new(TtyKind::PtySlave, index, slave_name.as_str()),
    );

    let master_payload = PayloadCap::from_cap(step_engine::sign_for(
        master_payload_res,
        TtyPayload::new_pty_master(slave.clone()),
    ));
    let slave_payload = PayloadCap::from_cap(step_engine::sign_for(
        slave_payload_res,
        TtyPayload::new_pty(master.clone()),
    ));
    master.install_payload(master_payload);
    slave.install_payload(slave_payload);

    if registry::register_pty_slave(index, slave.clone()).is_err() {
        return V3::Err(Errno::EIO.into());
    }

    let master_file = match project::open_file_for_tty(master.clone(), guard) {
        V3::Done(file) => file,
        V3::Err(err) => return V3::Err(err),
        _ => return V3::Err(Errno::EIO.into()),
    };
    let slave_file = match project::open_file_for_tty(slave.clone(), guard) {
        V3::Done(file) => file,
        V3::Err(err) => return V3::Err(err),
        _ => return V3::Err(Errno::EIO.into()),
    };

    V3::Done(OpenPtyOutcome {
        index,
        master,
        slave,
        master_file,
        slave_file,
    })
}

struct PtsName {
    buf: [u8; 16],
    len: usize,
}

impl PtsName {
    fn new(index: u32) -> Self {
        let mut out = Self {
            buf: [0; 16],
            len: 0,
        };
        out.push_bytes(b"pts/");
        out.push_u32(index);
        out
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("pts/?")
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.len < self.buf.len() {
                self.buf[self.len] = byte;
                self.len += 1;
            }
        }
    }

    fn push_u32(&mut self, value: u32) {
        let mut digits = [0u8; 10];
        let mut n = value;
        let mut len = 0;
        loop {
            digits[len] = b'0' + (n % 10) as u8;
            len += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for digit in digits[..len].iter().rev() {
            self.push_bytes(core::slice::from_ref(digit));
        }
    }
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 3)
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`step_openpty`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct OpenPtyOp<'a> {
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenPtyOp<'a> {
    type Output = OpenPtyOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_openpty(self.guard)
    }
}

#[cfg(test)]
mod step_op_wraps {
    use super::*;
    use crate::tty::adapter::step_engine::{
        PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome as V3,
    };

    use crate::test_support::EPOCH_TEST_LOCK;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = crate::zones::register_all();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    #[test]
    fn openpty_op_returns_done_with_pair() {
        let _setup = setup();
        let guard = step_engine::guard();
        let mut op = OpenPtyOp { guard: &guard };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        drop(guard);
        match outcome {
            V3::Done(_) => {}
            other => panic!("expected Done(OpenPtyOutcome), got {other:?}"),
        }
    }
}
