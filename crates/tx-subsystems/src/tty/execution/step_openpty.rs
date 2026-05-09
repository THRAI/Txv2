//! Minimal pty pair creation.

use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::execution::{Errno, Guard, StepOutcome};
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
pub fn step_openpty(
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<OpenPtyOutcome, tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;

    let index = match registry::allocate_pty_index() {
        Ok(index) => index,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    if registry::contains_pty_slave(index) {
        return V3::Err(Errno::EIO.into());
    }

    let master_id_res = match zone::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let slave_id_res = match zone::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let master_payload_res = match zone::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };
    let slave_payload_res = match zone::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return V3::Err(Errno::EIO.into()),
    };

    let master = zone::sign_for(
        master_id_res,
        TtyIdentity::new(TtyKind::PtyMaster, index, "ptmx"),
    );
    let slave_name = PtsName::new(index);
    let slave = zone::sign_for(
        slave_id_res,
        TtyIdentity::new(TtyKind::PtySlave, index, slave_name.as_str()),
    );

    let master_payload = PayloadCap::from_cap(zone::sign_for(
        master_payload_res,
        TtyPayload::new_pty_master(slave.clone()),
    ));
    let slave_payload = PayloadCap::from_cap(zone::sign_for(
        slave_payload_res,
        TtyPayload::new_pty(master.clone()),
    ));
    master.install_payload(master_payload);
    slave.install_payload(slave_payload);

    if registry::register_pty_slave(index, slave.clone()).is_err() {
        return V3::Err(Errno::EIO.into());
    }

    let master_file = match project::open_file_for_tty(master.clone(), guard) {
        StepOutcome::Done(file) => file,
        StepOutcome::Err(err) => return V3::Err(err.into()),
        _ => return V3::Err(Errno::EIO.into()),
    };
    let slave_file = match project::open_file_for_tty(slave.clone(), guard) {
        StepOutcome::Done(file) => file,
        StepOutcome::Err(err) => return V3::Err(err.into()),
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
