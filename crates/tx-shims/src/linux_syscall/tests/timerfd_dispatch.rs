// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, NR_READ, NR_TIMERFD_CREATE, NR_TIMERFD_GETTIME,
    NR_TIMERFD_SETTIME, TFD_TIMER_ABSTIME_FLAG, TFD_TIMER_CANCEL_ON_SET_FLAG,
};
use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
    TimerToken,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

const E_INVAL: i32 = 22;
const E_CANCELED: i32 = 125;

static TIMERFD_REF_POST_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static TIMERFD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TimerfdCountWake {
    wakes: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
}

impl alloc::task::Wake for TimerfdCountWake {
    fn wake(self: alloc::sync::Arc<Self>) {
        self.wakes
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }

    fn wake_by_ref(self: &alloc::sync::Arc<Self>) {
        self.wakes
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }
}

fn timerfd_counting_waker(
    wakes: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
) -> core::task::Waker {
    core::task::Waker::from(alloc::sync::Arc::new(TimerfdCountWake { wakes }))
}

struct TimerfdTestSetup {
    _lock: std::sync::MutexGuard<'static, ()>,
    _setup: TestSetup,
}

fn counting_timerfd_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    TIMERFD_REF_POST_COUNT.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    mailbox.post(event)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestItimerspec {
    it_interval: TestTimespec,
    it_value: TestTimespec,
}

fn timerfd_setup() -> (TimerfdTestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let lock = TIMERFD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (
        TimerfdTestSetup {
            _lock: lock,
            _setup: setup,
        },
        proc_cap,
        thread,
    )
}

fn create_timerfd(ctx: &SyscallCtx<'_>) -> i64 {
    create_timerfd_with_clock(ctx, CLOCK_MONOTONIC)
}

fn create_timerfd_with_clock(ctx: &SyscallCtx<'_>, clockid: u32) -> i64 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMERFD_CREATE, [clockid as u64, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("timerfd_create: {other:?}"),
    }
}

fn ns_from_timespec(ts: TestTimespec) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

fn assert_ns_near(actual: u64, expected: u64, context: &str) {
    assert!(
        actual <= expected && actual >= expected.saturating_sub(1_000_000),
        "{context}: expected close to {expected}ns, got {actual}ns",
    );
}

struct RecordedDeadline {
    token: TimerToken,
    deadline: DeadlineNs,
    target: TimerTarget,
}

#[derive(Default)]
struct DeadlineDomainTestDouble {
    next_token: core::sync::atomic::AtomicU64,
    deadlines: std::sync::Mutex<Vec<RecordedDeadline>>,
}

impl DeadlineDomainTestDouble {
    fn next_deadline(&self) -> Option<u64> {
        self.deadlines
            .lock()
            .expect("deadline domain lock")
            .iter()
            .map(|deadline| deadline.deadline.raw())
            .min()
    }

    fn fire_due(&self, now_ns: u64) -> usize {
        let due = {
            let mut deadlines = self.deadlines.lock().expect("deadline domain lock");
            let mut due = Vec::new();
            let mut index = 0;
            while index < deadlines.len() {
                if deadlines[index].deadline.raw() <= now_ns {
                    due.push(deadlines.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            due
        };

        for deadline in &due {
            match &deadline.target {
                TimerTarget::WaitSource { source, interests } => {
                    let source = tx_subsystems::wait_source::lookup_wait_source(source.raw())
                        .expect("timerfd deadline wait source must stay registered");
                    let _ = source.notify(*interests);
                }
                _ => panic!("timerfd tests only fire wait-source deadlines"),
            }
        }
        due.len()
    }
}

impl DeadlineDomain for DeadlineDomainTestDouble {
    fn register_deadline(
        &self,
        deadline: DeadlineNs,
        _role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let token = TimerToken::new(
            self.next_token
                .fetch_add(1, core::sync::atomic::Ordering::AcqRel)
                + 1,
        );
        self.deadlines
            .lock()
            .expect("deadline domain lock")
            .push(RecordedDeadline {
                token,
                deadline,
                target,
            });
        Ok(token)
    }

    fn cancel_deadline(&self, token: TimerToken) -> bool {
        let mut deadlines = self.deadlines.lock().expect("deadline domain lock");
        let Some(index) = deadlines
            .iter()
            .position(|deadline| deadline.token == token)
        else {
            return false;
        };
        deadlines.swap_remove(index);
        true
    }

    fn rearm_deadline(&self, token: TimerToken, deadline: DeadlineNs) -> bool {
        let mut deadlines = self.deadlines.lock().expect("deadline domain lock");
        let Some(existing) = deadlines
            .iter_mut()
            .find(|existing| existing.token == token)
        else {
            return false;
        };
        existing.deadline = deadline;
        true
    }
}

#[test]
fn dispatch_timerfd_settime_and_gettime_use_musl_itimerspec_layout() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);

    let first = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 0,
            tv_nsec: 500_000_000,
        },
        it_value: TestTimespec {
            tv_sec: 2,
            tv_nsec: 0,
        },
    };
    let first_ptr = &first as *const TestItimerspec as u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMERFD_SETTIME, [fd as u64, 0, first_ptr, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let second = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        it_value: TestTimespec {
            tv_sec: 3,
            tv_nsec: 0,
        },
    };
    let mut old = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &second as *const TestItimerspec as u64,
                &mut old as *mut TestItimerspec as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(ns_from_timespec(old.it_interval), 500_000_000);
    assert_ns_near(
        ns_from_timespec(old.it_value),
        2_000_000_000,
        "old.it_value",
    );

    let mut current = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_GETTIME,
            [
                fd as u64,
                &mut current as *mut TestItimerspec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(ns_from_timespec(current.it_interval), 1_000_000_000);
    assert_ns_near(
        ns_from_timespec(current.it_value),
        3_000_000_000,
        "current.it_value",
    );
}

#[test]
fn dispatch_timerfd_settime_registers_object_deadline_with_syscall_timer_registrar() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let domain = alloc::sync::Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        domain.next_deadline().is_some(),
        "timerfd_settime should register the object deadline in the unified timer registry"
    );

    let disarm = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &disarm as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        domain.next_deadline(),
        None,
        "disarming timerfd should drop its TimerGuard and cancel the registry deadline"
    );
}

#[test]
fn dispatch_timerfd_periodic_read_rearms_object_deadline_in_timer_registry() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let domain = alloc::sync::Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 0,
            tv_nsec: 100,
        },
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 10,
        },
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_TIMERFD_SETTIME,
                [
                    fd as u64,
                    0,
                    &new_value as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let first_deadline = domain
        .next_deadline()
        .expect("periodic timerfd arm should register first deadline");

    let mut blocked_out = 0u64;
    let blocked_read = SyscallRequest::new(
        NR_READ,
        [fd as u64, &mut blocked_out as *mut u64 as u64, 8, 0, 0, 0],
    );
    let wake_count = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let waker = timerfd_counting_waker(alloc::sync::Arc::clone(&wake_count));
    let mut poll_cx = Context::from_waker(&waker);
    let mut blocked_read = Box::pin(dispatch::<ShimsTestPmap>(blocked_read, &ctx));
    assert!(
        matches!(blocked_read.as_mut().poll(&mut poll_cx), Poll::Pending),
        "timerfd read should wait on the object-owned wait source"
    );

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(
        domain.fire_due(first_deadline + 1),
        1,
        "a blocked read must not install a competing generic deadline"
    );
    assert_eq!(
        wake_count.load(core::sync::atomic::Ordering::SeqCst),
        1,
        "TimerTarget::WaitSource must wake the subscribed timerfd read"
    );
    assert_eq!(
        blocked_read.as_mut().poll(&mut poll_cx),
        Poll::Ready(SyscallResult::Return(8)),
        "the original timerfd read must resume after the wait-source wake"
    );
    assert_eq!(blocked_out, 1);
    assert_eq!(
        domain.next_deadline(),
        Some(first_deadline + 100),
        "periodic timerfd read must re-register the next object deadline in the unified timer registry"
    );
}

#[test]
fn dispatch_timerfd_settime_rejects_unknown_flags() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0x8000_0000,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_timerfd_settime_rejects_invalid_nsec() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1_000_000_000,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_timerfd_settime_uses_syscall_ctx_mailbox_ref_post_for_readable_wake() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    TIMERFD_REF_POST_COUNT.store(0, core::sync::atomic::Ordering::Release);
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_mailbox_ref_post(counting_timerfd_ref_post);
    let fd = create_timerfd(&ctx);
    let mut out = 0u64;

    let read_req = SyscallRequest::new(
        NR_READ,
        [fd as u64, &mut out as *mut u64 as u64, 8, 0, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut read = Box::pin(dispatch::<ShimsTestPmap>(read_req, &ctx));
    assert!(
        matches!(read.as_mut().poll(&mut cx), Poll::Pending),
        "read on a disarmed blocking timerfd should park on the wait source"
    );

    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1,
        },
    };
    let settime_result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                TFD_TIMER_ABSTIME_FLAG as u64,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(settime_result, SyscallResult::Return(0));
    assert_eq!(
        TIMERFD_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        1,
        "timerfd settime should wake the parked reader through SyscallCtx"
    );
    assert_eq!(block_on(read), SyscallResult::Return(8));
    assert_eq!(out, 1);
}

#[test]
fn dispatch_timerfd_cancel_on_set_realtime_abstime_read_returns_ecanceled() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd_with_clock(&ctx, CLOCK_REALTIME);
    let target_ns = tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 20_000_000_000;
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: (target_ns / 1_000_000_000) as i64,
            tv_nsec: (target_ns % 1_000_000_000) as i64,
        },
    };
    let flags = TFD_TIMER_ABSTIME_FLAG | TFD_TIMER_CANCEL_ON_SET_FLAG;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                flags as u64,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let shifted_realtime = TestTimespec {
        tv_sec: ((tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 10_000_000_000)
            / 1_000_000_000) as i64,
        tv_nsec: 0,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            crate::linux_syscall::NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &shifted_realtime as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut buf = 0u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_READ,
            [fd as u64, &mut buf as *mut u64 as u64, 8, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_CANCELED));
}

#[test]
fn dispatch_clock_settime_cancel_on_set_cancels_timerfd_registry_guard() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let domain = alloc::sync::Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let fd = create_timerfd_with_clock(&ctx, CLOCK_REALTIME);
    let target_ns = tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 20_000_000_000;
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: (target_ns / 1_000_000_000) as i64,
            tv_nsec: (target_ns % 1_000_000_000) as i64,
        },
    };
    let flags = TFD_TIMER_ABSTIME_FLAG | TFD_TIMER_CANCEL_ON_SET_FLAG;

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_TIMERFD_SETTIME,
                [
                    fd as u64,
                    flags as u64,
                    &new_value as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(
        domain.next_deadline().is_some(),
        "cancel-on-set timerfd arm should register an object deadline"
    );

    let shifted_realtime = TestTimespec {
        tv_sec: ((tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 10_000_000_000)
            / 1_000_000_000) as i64,
        tv_nsec: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                crate::linux_syscall::NR_CLOCK_SETTIME,
                [
                    CLOCK_REALTIME as u64,
                    &shifted_realtime as *const TestTimespec as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        domain.next_deadline(),
        None,
        "cancel-on-set should drop the timerfd guard and remove the stale registry deadline"
    );
}

#[test]
fn dispatch_clock_settime_cancel_on_set_uses_syscall_ctx_mailbox_ref_post() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    TIMERFD_REF_POST_COUNT.store(0, core::sync::atomic::Ordering::Release);
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_mailbox_ref_post(counting_timerfd_ref_post);
    let fd = create_timerfd_with_clock(&ctx, CLOCK_REALTIME);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1_900_000_000,
            tv_nsec: 0,
        },
    };
    let flags = TFD_TIMER_ABSTIME_FLAG | TFD_TIMER_CANCEL_ON_SET_FLAG;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                flags as u64,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        TIMERFD_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        0,
        "future realtime timerfd arm should not publish readability yet"
    );

    let mut buf = 0u64;
    let read_req = SyscallRequest::new(
        NR_READ,
        [fd as u64, &mut buf as *mut u64 as u64, 8, 0, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut read = Box::pin(dispatch::<ShimsTestPmap>(read_req, &ctx));
    assert!(
        matches!(read.as_mut().poll(&mut cx), Poll::Pending),
        "read on a future realtime timerfd should park on the wait source"
    );

    let set = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 0,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            crate::linux_syscall::NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &set as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        TIMERFD_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        1,
        "clock_settime cancel-on-set should wake timerfd reader through SyscallCtx"
    );
    assert_eq!(block_on(read), SyscallResult::Error(E_CANCELED));
}

#[test]
fn dispatch_clock_settime_revalidates_realtime_timerfd_registry_deadline() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let domain = alloc::sync::Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let fd = create_timerfd_with_clock(&ctx, CLOCK_REALTIME);
    let target_ns = tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 20_000_000_000;
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: (target_ns / 1_000_000_000) as i64,
            tv_nsec: (target_ns % 1_000_000_000) as i64,
        },
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_TIMERFD_SETTIME,
                [
                    fd as u64,
                    TFD_TIMER_ABSTIME_FLAG as u64,
                    &new_value as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let initial_deadline = domain
        .next_deadline()
        .expect("realtime timerfd arm should register initial deadline");

    let shifted_realtime = TestTimespec {
        tv_sec: ((tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS + 10_000_000_000)
            / 1_000_000_000) as i64,
        tv_nsec: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                crate::linux_syscall::NR_CLOCK_SETTIME,
                [
                    CLOCK_REALTIME as u64,
                    &shifted_realtime as *const TestTimespec as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let revalidated_deadline = domain
        .next_deadline()
        .expect("clock_settime revalidation should keep the timerfd registered");
    assert_ne!(
        revalidated_deadline, initial_deadline,
        "clock_settime should replace the timerfd registry guard with the rebased realtime deadline"
    );
    assert!(
        revalidated_deadline < initial_deadline,
        "moving realtime forward should pull the monotonic timerfd deadline earlier"
    );
}
