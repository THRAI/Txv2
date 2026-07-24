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
use adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

pub const EFD_CLOEXEC: u32 = 0o2000000;
pub const EFD_NONBLOCK: u32 = 0o4000;
pub const EFD_SEMAPHORE: u32 = 0x1;

pub const EVENTFD_MAX: u64 = u64::MAX - 1;

pub struct EventFd {
    counter: AtomicU64,
    flags: AtomicU64,
    reader_source_id: u64,
    writer_source_id: u64,
    reader_source: Arc<WaitSource>,
    writer_source: Arc<WaitSource>,
}

impl EventFd {
    pub fn new(init_val: u64, flags: u32) -> Self {
        let wait_points = notification::new_wait_points();
        let reader_source_id =
            tx_substrate::wake::WaitEndpoint::source_id(wait_points.reader_endpoint()).raw();
        let writer_source_id =
            tx_substrate::wake::WaitEndpoint::source_id(wait_points.writer_endpoint()).raw();

        let readable = init_val != 0;
        let writable = init_val < EVENTFD_MAX;

        if readable {
            notification::notify_readable(wait_points.reader_endpoint());
        }
        if writable {
            notification::notify_writable(wait_points.writer_endpoint());
        }

        let (_, reader_source, _, writer_source) = wait_points.into_parts();
        EventFd {
            counter: AtomicU64::new(init_val),
            flags: AtomicU64::new(flags as u64),
            reader_source_id,
            writer_source_id,
            reader_source,
            writer_source,
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
    pub fn reader_endpoint(&self) -> &Arc<WaitSource> {
        &self.reader_source
    }
    pub fn writer_endpoint(&self) -> &Arc<WaitSource> {
        &self.writer_source
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
        wait_source::release_wait_source(self.reader_source_id);
        wait_source::release_wait_source(self.writer_source_id);
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

pub fn step_eventfd_read_with_post<F>(
    efd: &EventFd,
    out: &mut [u8; 8],
    nonblocking: bool,
    mut post: F,
) -> ByteOutcome
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
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
                return notification::wait_until_readable(efd.reader_endpoint());
            }
            if efd
                .counter
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                out.copy_from_slice(&1u64.to_le_bytes());
                if current > EVENTFD_MAX {
                    efd.fire_writable_with_post(&mut post);
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
        return notification::wait_until_readable(efd.reader_endpoint());
    }
    out.copy_from_slice(&val.to_le_bytes());
    efd.fire_writable_with_post(post);
    ByteOutcome::done(8)
}

pub fn step_eventfd_write_with_post<F>(
    efd: &EventFd,
    val: u64,
    nonblocking: bool,
    post: F,
) -> StepOutcome<(), NoProgress>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
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
            return notification::wait_until_writable(efd.writer_endpoint());
        }
        let new_val = current + val;
        if efd
            .counter
            .compare_exchange(current, new_val, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if current == 0 && new_val > 0 {
                efd.fire_readable_with_post(post);
            }
            return StepOutcome::done(());
        }
    }
}

impl EventFd {
    fn fire_readable_with_post<F>(&self, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        notification::notify_readable_with_post(&self.reader_source, post);
    }
    fn fire_writable_with_post<F>(&self, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        notification::notify_writable_with_post(&self.writer_source, post);
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

pub struct EventfdReadOp<'a, F>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    pub efd: &'a EventFd,
    pub out: &'a mut [u8; 8],
    pub nonblocking: bool,
    pub post: F,
}
impl<I: SubjectIdentity, F> StepOp<I> for EventfdReadOp<'_, F>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        step_eventfd_read_with_post(self.efd, self.out, self.nonblocking, &mut self.post)
    }
}

pub struct EventfdWriteOp<'a, F>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    pub efd: &'a EventFd,
    pub val: u64,
    pub nonblocking: bool,
    pub post: F,
}
impl<I: SubjectIdentity, F> StepOp<I> for EventfdWriteOp<'_, F>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        step_eventfd_write_with_post(self.efd, self.val, self.nonblocking, &mut self.post)
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

    fn direct_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
        mailbox.post(event)
    }

    #[test]
    fn endpoints_match_registered_reader_and_writer_sources() {
        let _g = setup();
        let cap = eventfd_create(0, 0).expect("create");

        let reader = cap.reader_endpoint();
        let writer = cap.writer_endpoint();

        assert_eq!(
            tx_substrate::wake::WaitEndpoint::source_id(reader).raw(),
            cap.reader_source_id()
        );
        assert_eq!(
            tx_substrate::wake::WaitEndpoint::source_id(writer).raw(),
            cap.writer_source_id()
        );
        assert!(crate::wait_source::lookup_wait_source(cap.reader_source_id()).is_some());
        assert!(crate::wait_source::lookup_wait_source(cap.writer_source_id()).is_some());
    }

    #[test]
    fn create_with_zero_counter_reads_eagain_nonblocking() {
        let _g = setup();
        let cap = eventfd_create(0, EFD_NONBLOCK).expect("create");
        let mut buf = [0u8; 8];
        match step_eventfd_read_with_post(&cap, &mut buf, true, direct_post) {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected EAGAIN, got {other:?}"),
        }
    }

    #[test]
    fn create_with_nonzero_counter_reads_value_and_resets() {
        let _g = setup();
        let cap = eventfd_create(42, 0).expect("create");
        let mut buf = [0xFFu8; 8];
        match step_eventfd_read_with_post(&cap, &mut buf, false, direct_post) {
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
        assert_eq!(
            step_eventfd_write_with_post(&cap, 5, false, direct_post),
            StepOutcome::done(())
        );
        assert_eq!(cap.counter(), 15);
    }

    #[test]
    fn write_read_roundtrip() {
        let _g = setup();
        let cap = eventfd_create(0, 0).expect("create");
        assert_eq!(
            step_eventfd_write_with_post(&cap, 7, false, direct_post),
            StepOutcome::done(())
        );
        let mut buf = [0u8; 8];
        match step_eventfd_read_with_post(&cap, &mut buf, false, direct_post) {
            StepOutcome::Done(8) => assert_eq!(u64::from_le_bytes(buf), 7),
            other => panic!("expected Done(8), got {other:?}"),
        }
    }

    #[test]
    fn semaphore_mode_read_returns_one() {
        let _g = setup();
        let cap = eventfd_create(5, EFD_SEMAPHORE).expect("create");
        let mut buf = [0u8; 8];
        match step_eventfd_read_with_post(&cap, &mut buf, false, direct_post) {
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
            step_eventfd_write_with_post(&cap, 0, false, direct_post),
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
