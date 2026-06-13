//! `eventfd(2)` — create a file descriptor for event notification.
//!
//! Spec: `man 2 eventfd`.  An eventfd object contains an unsigned
//! 64-bit integer counter maintained by the kernel.  `write(2)` adds
//! the value to the counter (blocking if the addition would overflow
//! `u64::MAX - 1`); `read(2)` returns the current counter value and
//! resets it to zero (or, with `EFD_SEMAPHORE`, returns 1 and
//! decrements by 1).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;
pub mod notification;

use crate::wait_source;
use adapter::step_engine::{
    eagain, eagain_no_progress, sign, ByteOutcome, ByteProgress, Cap, NoProgress, OneShotStepOp,
    ScriptCtx, StepOp, StepOutcome, SubjectIdentity, V3Errno, WaitSource, Zone, ZoneAllocated,
    ZoneError,
};
use adapter::wait_routing::{self, Channel};

pub const EFD_CLOEXEC: u32 = 0o2000000;
pub const EFD_NONBLOCK: u32 = 0o4000;
pub const EFD_SEMAPHORE: u32 = 0x1;

pub const EVENTFD_MAX: u64 = u64::MAX - 1;

pub struct EventFd {
    counter: AtomicU64,
    flags: AtomicU64,
    reader_source_id: u64,
    writer_source_id: u64,
    reader_channel: Option<Channel>,
    writer_channel: Option<Channel>,
    reader_source: Option<Arc<WaitSource>>,
    writer_source: Option<Arc<WaitSource>>,
}

impl EventFd {
    pub fn new(init_val: u64, flags: u32) -> Self {
        let wait_points = notification::new_wait_points();

        let readable = init_val != 0;
        let writable = init_val < EVENTFD_MAX;

        if readable {
            notification::notify_readable(
                Some(&wait_points.reader_channel),
                Some(&wait_points.reader_source),
            );
        }
        if writable {
            notification::notify_writable(
                Some(&wait_points.writer_channel),
                Some(&wait_points.writer_source),
            );
        }

        EventFd {
            counter: AtomicU64::new(init_val),
            flags: AtomicU64::new(flags as u64),
            reader_source_id: wait_points.reader_source_id,
            writer_source_id: wait_points.writer_source_id,
            reader_channel: Some(wait_points.reader_channel),
            writer_channel: Some(wait_points.writer_channel),
            reader_source: Some(wait_points.reader_source),
            writer_source: Some(wait_points.writer_source),
        }
    }

    pub fn counter(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire) as u32
    }
    pub fn reader_source_id(&self) -> u64 {
        self.reader_source_id
    }
    pub fn writer_source_id(&self) -> u64 {
        self.writer_source_id
    }
    fn is_semaphore(&self) -> bool {
        (self.flags() & EFD_SEMAPHORE) != 0
    }
    #[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
    fn is_nonblocking(&self) -> bool {
        (self.flags() & EFD_NONBLOCK) != 0
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        wait_source::release_wait_channel(self.reader_source_id);
        wait_source::release_wait_channel(self.writer_source_id);
        wait_routing::unregister_source(self.reader_source_id);
        wait_routing::unregister_source(self.writer_source_id);
    }
}

static EVENTFD_ZONE: Zone<EventFd> = Zone::const_new();

unsafe impl ZoneAllocated for EventFd {
    fn zone() -> &'static Zone<Self> {
        &EVENTFD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<EventFd>()?;
    Ok(())
}

pub fn eventfd_create(init_val: u64, flags: u32) -> Result<Cap<EventFd>, ZoneError> {
    let efd = EventFd::new(init_val, flags);
    sign(efd)
}

pub fn step_eventfd_read(efd: &EventFd, out: &mut [u8; 8], nonblocking: bool) -> ByteOutcome {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if efd.is_semaphore() {
        loop {
            let current = efd.counter.load(Ordering::Acquire);
            if current == 0 {
                if nonblocking {
                    return eagain();
                }
                return notification::wait_until_readable(efd.reader_source_id);
            }
            if efd
                .counter
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                out.copy_from_slice(&1u64.to_le_bytes());
                if current > EVENTFD_MAX {
                    efd.fire_writable();
                }
                return ByteOutcome::done(8);
            }
        }
    }
    let val = efd.counter.swap(0, Ordering::AcqRel);
    if val == 0 {
        if nonblocking {
            return eagain();
        }
        return notification::wait_until_readable(efd.reader_source_id);
    }
    out.copy_from_slice(&val.to_le_bytes());
    efd.fire_writable();
    ByteOutcome::done(8)
}

pub fn step_eventfd_write(
    efd: &EventFd,
    val: u64,
    nonblocking: bool,
) -> StepOutcome<(), NoProgress> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if val == 0 || val == u64::MAX {
        return StepOutcome::Err(V3Errno::EINVAL);
    }
    loop {
        let current = efd.counter.load(Ordering::Acquire);
        let max_add = EVENTFD_MAX.saturating_sub(current);
        if val > max_add {
            if nonblocking {
                return eagain_no_progress();
            }
            return notification::wait_until_writable(efd.writer_source_id);
        }
        let new_val = current + val;
        if efd
            .counter
            .compare_exchange(current, new_val, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if current == 0 && new_val > 0 {
                efd.fire_readable();
            }
            return StepOutcome::done(());
        }
    }
}

impl EventFd {
    fn fire_readable(&self) {
        notification::notify_readable(self.reader_channel.as_ref(), self.reader_source.as_ref());
    }
    fn fire_writable(&self) {
        notification::notify_writable(self.writer_channel.as_ref(), self.writer_source.as_ref());
    }
}

pub struct EventfdCreateOp {
    pub init_val: u64,
    pub flags: u32,
}
impl<I: SubjectIdentity> StepOp<I> for EventfdCreateOp {
    type Output = Result<Cap<EventFd>, ZoneError>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(eventfd_create(self.init_val, self.flags))
    }
}
impl<I: SubjectIdentity> OneShotStepOp<I> for EventfdCreateOp {}

pub struct EventfdReadOp<'a> {
    pub efd: &'a EventFd,
    pub out: &'a mut [u8; 8],
    pub nonblocking: bool,
}
impl<I: SubjectIdentity> StepOp<I> for EventfdReadOp<'_> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_eventfd_read(self.efd, self.out, self.nonblocking)
    }
}

pub struct EventfdWriteOp<'a> {
    pub efd: &'a EventFd,
    pub val: u64,
    pub nonblocking: bool,
}
impl<I: SubjectIdentity> StepOp<I> for EventfdWriteOp<'_> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_eventfd_write(self.efd, self.val, self.nonblocking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessIdentity;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
    }

    #[test]
    fn create_with_zero_counter_reads_eagain_nonblocking() {
        let _g = setup();
        let cap = eventfd_create(0, EFD_NONBLOCK).expect("create");
        let mut buf = [0u8; 8];
        match step_eventfd_read(&cap, &mut buf, true) {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected EAGAIN, got {other:?}"),
        }
    }

    #[test]
    fn create_with_nonzero_counter_reads_value_and_resets() {
        let _g = setup();
        let cap = eventfd_create(42, 0).expect("create");
        let mut buf = [0xFFu8; 8];
        match step_eventfd_read(&cap, &mut buf, false) {
            StepOutcome::Done(n) => {
                assert_eq!(n, 8);
                assert_eq!(u64::from_le_bytes(buf), 42);
            }
            other => panic!("expected Done(8), got {other:?}"),
        }
        assert_eq!(cap.counter(), 0);
    }

    #[test]
    fn write_adds_to_counter() {
        let _g = setup();
        let cap = eventfd_create(10, 0).expect("create");
        assert_eq!(step_eventfd_write(&cap, 5, false), StepOutcome::done(()));
        assert_eq!(cap.counter(), 15);
    }

    #[test]
    fn write_read_roundtrip() {
        let _g = setup();
        let cap = eventfd_create(0, 0).expect("create");
        assert_eq!(step_eventfd_write(&cap, 7, false), StepOutcome::done(()));
        let mut buf = [0u8; 8];
        match step_eventfd_read(&cap, &mut buf, false) {
            StepOutcome::Done(8) => assert_eq!(u64::from_le_bytes(buf), 7),
            other => panic!("expected Done(8), got {other:?}"),
        }
    }

    #[test]
    fn semaphore_mode_read_returns_one() {
        let _g = setup();
        let cap = eventfd_create(5, EFD_SEMAPHORE).expect("create");
        let mut buf = [0u8; 8];
        match step_eventfd_read(&cap, &mut buf, false) {
            StepOutcome::Done(8) => assert_eq!(u64::from_le_bytes(buf), 1),
            other => panic!("expected Done(8), got {other:?}"),
        }
        assert_eq!(cap.counter(), 4);
    }

    #[test]
    fn write_einval_for_zero() {
        let _g = setup();
        let cap = eventfd_create(0, 0).expect("create");
        assert_eq!(
            step_eventfd_write(&cap, 0, false),
            StepOutcome::Err(V3Errno::EINVAL)
        );
    }

    #[test]
    fn create_op_delegates_to_free_fn() {
        let _g = setup();
        let mut op = EventfdCreateOp {
            init_val: 10,
            flags: 0,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            StepOutcome::Done(Ok(cap)) => assert_eq!(cap.counter(), 10),
            other => panic!("expected Done(Ok(_)), got {other:?}"),
        }
    }
}
