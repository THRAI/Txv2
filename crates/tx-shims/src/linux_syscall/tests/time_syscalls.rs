// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use alloc::{
    sync::{Arc, Weak as ArcWeak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_CLOCK_SETTIME, NR_GETITIMER, NR_GETPID,
    NR_GETTIMEOFDAY, NR_NANOSLEEP, NR_SETITIMER, NR_SETTIMEOFDAY, NR_TIMES, TIMER_ABSTIME,
    TIMES_NS_PER_TICK,
};
use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
    TimerToken,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_subsystems::cred::{step_setresuid, Uid};
use tx_subsystems::signal::{SigDisposition, Signum};

const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_PERM: i32 = 1;
const OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC: i64 = 1_779_473_960;
static ITIMER_SIGNAL_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

fn counting_itimer_post(mailbox: ArcWeak<TaskMailbox>, event: MailboxEvent) {
    ITIMER_SIGNAL_POST_COUNT.fetch_add(1, Ordering::SeqCst);
    if let Some(mailbox) = mailbox.upgrade() {
        let _ = mailbox.post(event);
    }
}

struct RecordedDeadline {
    token: TimerToken,
    deadline: DeadlineNs,
    role: TimerRole,
    target: TimerTarget,
}

#[derive(Default)]
struct DeadlineDomainTestDouble {
    next_token: AtomicU64,
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
                TimerTarget::TaskMailbox(mailbox) => {
                    if let Some(mailbox) = mailbox.upgrade() {
                        let _ = mailbox.post(MailboxEvent::TimerFired {
                            token: deadline.token,
                        });
                    }
                }
                TimerTarget::SignalTarget { mailbox } => {
                    if let Some(mailbox) = mailbox.upgrade() {
                        let _ = mailbox.post(MailboxEvent::SignalTimerFired {
                            token: deadline.token,
                        });
                    }
                }
                _ => panic!("time syscall tests only fire mailbox-backed deadlines"),
            }
        }
        due.len()
    }
}

impl DeadlineDomain for DeadlineDomainTestDouble {
    fn register_deadline(
        &self,
        deadline: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let token = TimerToken::new(self.next_token.fetch_add(1, Ordering::AcqRel) + 1);
        self.deadlines
            .lock()
            .expect("deadline domain lock")
            .push(RecordedDeadline {
                token,
                deadline,
                role,
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

/// Mirror of `TimespecLayout` for test-side decoding. The
/// production layout is private to `mod.rs`, so the tests
/// reconstruct the same shape via `read_volatile` against a
/// stack-allocated buffer.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestItimerval {
    it_interval: TestTimeval,
    it_value: TestTimeval,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTms {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

fn time_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

/// `clock_gettime(CLOCK_MONOTONIC, ts)` succeeds and writes a
/// `(tv_sec, tv_nsec)` pair derived from the platform clock.
/// `ShimsTestPmap::read_ns()` starts at 5_000_000_000 ns
/// (= 5 seconds) and increments per call, so the observed
/// timespec must satisfy `tv_sec >= 5` and `tv_nsec` is in
/// `[0, 1_000_000_000)`.
#[test]
fn dispatch_clock_gettime_monotonic_writes_timespec_to_user() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_GETTIME,
        [CLOCK_MONOTONIC as u64, ts_uaddr, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(ts.tv_sec >= 5, "tv_sec should reflect the test clock base");
    assert!(
        (0..1_000_000_000).contains(&ts.tv_nsec),
        "tv_nsec must be in [0, 1e9): got {}",
        ts.tv_nsec,
    );
}

/// CPU-time clock ids alias to the platform monotonic in v1 and
/// must succeed.
#[test]
fn dispatch_clock_gettime_cputime_aliases_to_monotonic() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;
    for clk in [
        CLOCK_REALTIME,
        CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID,
    ] {
        let req = SyscallRequest::new(NR_CLOCK_GETTIME, [clk as u64, ts_uaddr, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0), "clk_id {clk}");
    }
}

/// Unrecognised clock ids return `-EINVAL`.
#[test]
fn dispatch_clock_gettime_invalid_clock_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;

    let req = SyscallRequest::new(NR_CLOCK_GETTIME, [99, ts_uaddr, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// Null `tp` returns `-EFAULT` (without dereferencing the null
/// pointer).
#[test]
fn dispatch_clock_gettime_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_CLOCK_GETTIME, [CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `gettimeofday(tv, _)` writes a `(tv_sec, tv_usec)` pair from
/// the platform clock.
#[test]
fn dispatch_gettimeofday_writes_timeval_to_user() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tv = TestTimeval::default();
    let tv_uaddr = &mut tv as *mut TestTimeval as u64;

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [tv_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(tv.tv_sec >= 5);
    assert!(
        (0..1_000_000).contains(&tv.tv_usec),
        "tv_usec must be in [0, 1e6): got {}",
        tv.tv_usec,
    );
}

#[test]
fn dispatch_setitimer_and_getitimer_round_trip_real_timer() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 2,
            tv_usec: 0,
        },
        it_value: TestTimeval {
            tv_sec: 10,
            tv_usec: 0,
        },
    };
    let mut old_timer = TestItimerval::default();
    let mut current = TestItimerval::default();

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL,
                &new_timer as *const TestItimerval as u64,
                &mut old_timer as *mut TestItimerval as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));
    assert_eq!(old_timer, TestItimerval::default());

    let get = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETITIMER,
            [
                ITIMER_REAL,
                &mut current as *mut TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(get, SyscallResult::Return(0));
    assert_eq!(current.it_interval.tv_sec, 2);
    assert_eq!(current.it_interval.tv_usec, 0);
    assert!(
        current.it_value.tv_sec > 0 || current.it_value.tv_usec > 0,
        "active interval timer should report non-zero remaining time"
    );
}

#[test]
fn dispatch_setitimer_registers_deadline_with_syscall_timer_registrar() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let new_timer = TestItimerval {
        it_interval: TestTimeval::default(),
        it_value: TestTimeval {
            tv_sec: 1,
            tv_usec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL,
                &new_timer as *const TestItimerval as u64,
                0,
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
        "setitimer should register its producer deadline in the unified timer registry"
    );
}

#[test]
fn dispatch_itimer_real_boundary_uses_syscall_ctx_mailbox_post() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let mailbox = Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::new(14).expect("SIGALRM"),
        SigDisposition::Handler(0xCAFE),
    );
    ITIMER_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap, thread).with_mailbox_post(counting_itimer_post);
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL,
                &new_timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));
    for _ in 0..1_100 {
        let _ = <ShimsTestPmap as tx_hal::MonotonicCounterIf>::read_ns();
    }

    let boundary = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETPID, [0; 6]),
        &ctx,
    ));

    assert_eq!(boundary, SyscallResult::Return(ctx.process.pid.0 as i64));
    assert_eq!(
        ITIMER_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "ITIMER_REAL boundary delivery should use SyscallCtx mailbox post"
    );
}

#[test]
fn dispatch_itimer_real_periodic_boundary_rearms_registry_deadline() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::new(14).expect("SIGALRM"),
        SigDisposition::Handler(0xCAFE),
    );
    ITIMER_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(mailbox)
        .with_mailbox_post(counting_itimer_post)
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 2,
        },
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL,
                &new_timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));
    let first_deadline = domain
        .next_deadline()
        .expect("periodic ITIMER_REAL should register first deadline");

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(domain.fire_due(first_deadline + 1), 1);
    assert_eq!(
        domain.next_deadline(),
        None,
        "manual fire removes the old ITIMER_REAL registry entry before syscall-boundary rearm"
    );

    let boundary = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETPID, [0; 6]),
        &ctx,
    ));

    assert_eq!(boundary, SyscallResult::Return(ctx.process.pid.0 as i64));
    assert_eq!(
        ITIMER_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "ITIMER_REAL boundary delivery should still deliver SIGALRM"
    );
    assert!(
        domain.next_deadline().is_some(),
        "periodic ITIMER_REAL boundary rearm should register the next deadline in the unified timer registry"
    );
}

#[test]
fn dispatch_itimer_real_poll_due_periodic_rearms_registry_deadline() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::new(14).expect("SIGALRM"),
        SigDisposition::Handler(0xCAFE),
    );
    ITIMER_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(mailbox)
        .with_mailbox_post(counting_itimer_post)
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 2,
        },
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL,
                &new_timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));
    let first_deadline = domain
        .next_deadline()
        .expect("periodic ITIMER_REAL should register first deadline");

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(domain.fire_due(first_deadline + 1), 1);
    assert_eq!(
        domain.next_deadline(),
        None,
        "manual fire removes the old ITIMER_REAL registry entry before poll_due rearm"
    );

    let registrar = ctx.timer_registrar.as_ref().unwrap().clone();
    let timer_mailbox = ctx
        .mailbox
        .as_ref()
        .map(Arc::downgrade)
        .expect("test context should carry a timer mailbox");
    let next_deadline = crate::linux_syscall::poll_due_itimers_with_post::<ShimsTestPmap, _>(
        &proc_cap,
        Some(&registrar),
        Some(timer_mailbox),
        |mailbox, event| {
            counting_itimer_post(mailbox, event);
        },
    );

    assert!(next_deadline.is_some());
    assert_eq!(
        ITIMER_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "ITIMER_REAL poll_due delivery should still deliver SIGALRM"
    );
    assert!(
        domain.next_deadline().is_some(),
        "periodic ITIMER_REAL poll_due rearm should register the next deadline in the unified timer registry"
    );
}

#[test]
fn dispatch_itimer_real_signal_timer_hint_waits_for_due_scan() {
    const ITIMER_REAL: u64 = 0;

    let (_setup, proc_cap, thread) = time_setup();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &proc_cap,
        Signum::new(14).expect("SIGALRM"),
        SigDisposition::Handler(0xCAFE),
    );
    ITIMER_SIGNAL_POST_COUNT.store(0, Ordering::SeqCst);
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(Arc::clone(&mailbox))
        .with_mailbox_post(counting_itimer_post)
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 2,
        },
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SETITIMER,
                [
                    ITIMER_REAL,
                    &new_timer as *const TestItimerval as u64,
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
    let first_deadline = domain
        .next_deadline()
        .expect("ITIMER_REAL should register first signal timer deadline");

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(domain.fire_due(first_deadline + 1), 1);
    assert_eq!(
        ITIMER_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        0,
        "SignalTimerFired is only a wake hint; it must not deliver SIGALRM before due scan"
    );
    assert_eq!(
        mailbox.len(),
        1,
        "timer registry fire should enqueue a SignalTimerFired hint",
    );

    let registrar = ctx.timer_registrar.as_ref().unwrap().clone();
    let timer_mailbox = ctx
        .mailbox
        .as_ref()
        .map(Arc::downgrade)
        .expect("test context should carry a timer mailbox");
    let next_deadline = crate::linux_syscall::poll_due_itimers_with_post::<ShimsTestPmap, _>(
        &proc_cap,
        Some(&registrar),
        Some(timer_mailbox),
        |mailbox, event| {
            counting_itimer_post(mailbox, event);
        },
    );

    assert!(next_deadline.is_some());
    assert_eq!(
        ITIMER_SIGNAL_POST_COUNT.load(Ordering::SeqCst),
        1,
        "due scan should deliver SIGALRM through the normal signal post path"
    );
}

#[test]
fn dispatch_gettimeofday_realtime_is_not_before_oscomp_image_timestamps() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tv = TestTimeval::default();
    let tv_uaddr = &mut tv as *mut TestTimeval as u64;

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [tv_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        tv.tv_sec >= OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC,
        "CLOCK_REALTIME seconds {} must not predate OSComp image mtimes {}",
        tv.tv_sec,
        OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC,
    );
}

/// Null `tv` returns `-EFAULT`.
#[test]
fn dispatch_gettimeofday_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

#[test]
fn dispatch_clock_settime_updates_realtime_without_moving_monotonic() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new_rt = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 123_456_789,
    };

    let before_mono = {
        let mut ts = TestTimespec::default();
        let result = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_CLOCK_GETTIME,
                [
                    CLOCK_MONOTONIC as u64,
                    &mut ts as *mut TestTimespec as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(result, SyscallResult::Return(0));
        ts.tv_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec)
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &new_rt as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let expected_rt_ns = (new_rt.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(new_rt.tv_nsec as u64);
    assert_eq!(
        SHIMS_TEST_RTC_SET_NS.load(core::sync::atomic::Ordering::Acquire),
        expected_rt_ns,
        "clock_settime should attempt best-effort persistent writeback"
    );

    let mut rt = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_REALTIME as u64,
                &mut rt as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(rt.tv_sec, new_rt.tv_sec);
    assert!(rt.tv_nsec >= new_rt.tv_nsec);

    let mut after = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_MONOTONIC as u64,
                &mut after as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let after_mono = after
        .tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(after.tv_nsec);
    assert!(after_mono >= before_mono);
    assert!(
        after_mono - before_mono < 1_000_000,
        "clock_settime must not jump CLOCK_MONOTONIC: before={before_mono} after={after_mono}"
    );
}

#[test]
fn dispatch_settimeofday_updates_gettimeofday_realtime() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let tv = TestTimeval {
        tv_sec: 1_800_000_010,
        tv_usec: 654_321,
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETTIMEOFDAY,
            [&tv as *const TestTimeval as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let expected_rt_ns = (tv.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((tv.tv_usec as u64).saturating_mul(1_000));
    assert_eq!(
        SHIMS_TEST_RTC_SET_NS.load(core::sync::atomic::Ordering::Acquire),
        expected_rt_ns,
        "settimeofday should attempt best-effort persistent writeback"
    );

    let mut out = TestTimeval::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETTIMEOFDAY,
            [&mut out as *mut TestTimeval as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out.tv_sec, tv.tv_sec);
    assert!(out.tv_usec >= tv.tv_usec);
}

#[test]
fn dispatch_clock_settime_ignores_persistent_writeback_failure_after_timekeeper_update() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    SHIMS_TEST_RTC_SET_FAIL.store(true, core::sync::atomic::Ordering::Release);
    let new_rt = TestTimespec {
        tv_sec: 1_800_000_020,
        tv_nsec: 222_333_444,
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &new_rt as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(
        result,
        SyscallResult::Return(0),
        "best-effort RTC writeback failure must not fail accepted system time mutation"
    );
    assert_eq!(
        SHIMS_TEST_RTC_SET_NS.load(core::sync::atomic::Ordering::Acquire),
        0,
        "failing fake persistent clock must not record a successful RTC write"
    );

    let mut rt = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_REALTIME as u64,
                &mut rt as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(rt.tv_sec, new_rt.tv_sec);
    assert!(rt.tv_nsec >= new_rt.tv_nsec);
}

#[test]
fn dispatch_clock_settime_rejects_non_realtime_and_unprivileged_callers() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let ts = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 0,
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_MONOTONIC as u64,
                &ts as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let target = Uid(1000);
    assert!(matches!(
        step_setresuid(&proc_cap, Some(target), Some(target), Some(target)),
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    let unpriv_ctx = make_ctx(proc_cap, thread);
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &ts as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &unpriv_ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_PERM));
}

/// `times(buf)` returns the monotonic tick count and writes
/// `tms_utime = ticks`. The other three fields stay at their
/// pre-existing values (the syscall arm zeros them, which is
/// observable since the buffer is initialised to a non-zero
/// sentinel below).
#[test]
fn dispatch_times_returns_tick_count_and_writes_buffer() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    // Initialise the buffer to a sentinel so we can assert the
    // syscall arm overwrites all four fields.
    let mut tms = TestTms {
        tms_utime: 0xdead_beef,
        tms_stime: 0xdead_beef,
        tms_cutime: 0xdead_beef,
        tms_cstime: 0xdead_beef,
    };
    let buf_uaddr = &mut tms as *mut TestTms as u64;

    let req = SyscallRequest::new(NR_TIMES, [buf_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let returned_ticks = match result {
        SyscallResult::Return(t) => t,
        other => panic!("expected Return, got {other:?}"),
    };
    assert!(returned_ticks > 0);
    assert_eq!(
        tms.tms_utime, returned_ticks,
        "tms_utime should match the returned tick count",
    );
    assert_eq!(tms.tms_stime, 0);
    assert_eq!(tms.tms_cutime, 0);
    assert_eq!(tms.tms_cstime, 0);

    // Sanity: returned_ticks * NS_PER_TICK should be in the same
    // ballpark as the test clock base (5 seconds = 500 ticks at
    // 100Hz) — bounded loosely so other tests advancing the
    // counter do not break this one.
    let approx_ns = (returned_ticks as u64) * TIMES_NS_PER_TICK;
    assert!(
        approx_ns >= 5_000_000_000,
        "ticks {returned_ticks} * {TIMES_NS_PER_TICK}ns = {approx_ns}ns < 5e9ns",
    );
}

/// `times(NULL)` returns the tick count without writing anywhere.
/// Linux semantics: a null `buf` is permitted; only the return
/// value matters in that case (LTP `times02`).
#[test]
fn dispatch_times_with_null_buffer_returns_tick_count() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_TIMES, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let ticks = match result {
        SyscallResult::Return(t) => t,
        other => panic!("expected Return, got {other:?}"),
    };
    assert!(ticks > 0);
}

/// `nanosleep((0, 0), _)` short-circuits to `Return(0)` per the
/// Linux semantics — a zero-duration sleep is a no-op.
#[test]
fn dispatch_nanosleep_zero_duration_returns_immediately() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `nanosleep((1, 0), _)` returns `0` — real-duration sleeps now
/// complete immediately in the test context (no reactor installed,
/// so the timer future is skipped and we return success).
#[test]
fn dispatch_nanosleep_nonzero_duration_returns_zero_without_reactor() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn dispatch_nanosleep_positive_duration_uses_unified_timer_registry() {
    let (_setup, proc_cap, thread) = time_setup();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(Arc::clone(&mailbox))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;
    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    let mut deadline = None;
    for _ in 0..4 {
        let poll = pinned.as_mut().poll(&mut cx);
        assert!(
            matches!(poll, Poll::Pending),
            "positive nanosleep should park on the unified timer before expiry; got {poll:?}"
        );
        deadline = domain.next_deadline();
        if deadline.is_some() {
            break;
        }
    }
    let deadline = deadline.expect("positive nanosleep should register a PrimarySleep timer");

    assert_eq!(domain.fire_due(deadline), 1);

    for _ in 0..16 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(0));
            return;
        }
    }
    panic!("nanosleep did not resolve after unified timer expiry");
}

/// `nanosleep((-1, 0), _)` returns `-EINVAL` — negative tv_sec is
/// rejected by Linux.
#[test]
fn dispatch_nanosleep_negative_tv_sec_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: -1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `nanosleep(NULL, _)` returns `-EFAULT`.
#[test]
fn dispatch_nanosleep_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_NANOSLEEP, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, past_ts, _)`
/// short-circuits to `Return(0)` because the absolute deadline
/// is already in the past.
#[test]
fn dispatch_clock_nanosleep_abstime_past_deadline_returns_immediately() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    // tv_sec = 1 (== 1e9 ns), well below the test clock base of
    // 5_000_000_000 ns — the deadline is already past.
    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_NANOSLEEP,
        [
            CLOCK_MONOTONIC as u64,
            TIMER_ABSTIME as u64,
            req_uaddr,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `clock_nanosleep` with an unknown clock id returns `-EINVAL`.
#[test]
fn dispatch_clock_nanosleep_invalid_clock_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_CLOCK_NANOSLEEP, [99, 0, req_uaddr, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `clock_nanosleep` with an unknown flag bit returns `-EINVAL`.
#[test]
fn dispatch_clock_nanosleep_unknown_flag_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_NANOSLEEP,
        [
            CLOCK_MONOTONIC as u64,
            0x2, // unknown flag bit
            req_uaddr,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}
