// POSIX timer syscall dispatch coverage.
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_TIMER_CREATE, NR_TIMER_DELETE, NR_TIMER_GETOVERRUN, NR_TIMER_GETTIME,
    NR_TIMER_SETTIME,
};
use alloc::{
    sync::{Arc, Weak as ArcWeak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};
use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
    TimerToken,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};
use tx_subsystems::signal::{SigDisposition, Signum};

const E_INVAL: i32 = 22;

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
                TimerTarget::SignalTarget { mailbox } => {
                    if let Some(mailbox) = mailbox.upgrade() {
                        let _ = mailbox.post(MailboxEvent::SignalTimerFired {
                            token: deadline.token,
                        });
                    }
                }
                _ => panic!("POSIX timer tests only accept SignalTarget deadlines"),
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

fn posix_timer_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

fn create_posix_timer(ctx: &SyscallCtx<'_>) -> i32 {
    let mut timerid = 0i32;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [
                CLOCK_MONOTONIC as u64,
                0,
                &mut timerid as *mut i32 as u64,
                0,
                0,
                0,
            ],
        ),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(timerid > 0, "timer_create should write a positive timer id");
    timerid
}

fn delete_posix_timer(ctx: &SyscallCtx<'_>, timerid: i32) {
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_DELETE, [timerid as u64, 0, 0, 0, 0, 0]),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
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

#[test]
fn dispatch_posix_timer_settime_gettime_and_delete_round_trip() {
    let (_setup, proc_cap, thread) = posix_timer_setup();
    let ctx = make_ctx(proc_cap, thread);
    let timerid = create_posix_timer(&ctx);

    let first = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 0,
            tv_nsec: 250_000_000,
        },
        it_value: TestTimespec {
            tv_sec: 2,
            tv_nsec: 0,
        },
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timerid as u64,
                0,
                &first as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
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
            NR_TIMER_SETTIME,
            [
                timerid as u64,
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
    assert_eq!(ns_from_timespec(old.it_interval), 250_000_000);
    assert_ns_near(
        ns_from_timespec(old.it_value),
        2_000_000_000,
        "old.it_value",
    );

    let mut current = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_GETTIME,
            [
                timerid as u64,
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

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_GETOVERRUN, [timerid as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_DELETE, [timerid as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_GETTIME,
            [
                timerid as u64,
                &mut current as *mut TestItimerspec as u64,
                0,
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
fn dispatch_posix_timer_settime_registers_deadline_with_syscall_timer_registrar() {
    let (_setup, proc_cap, thread) = posix_timer_setup();
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let timerid = create_posix_timer(&ctx);
    let first = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timerid as u64,
                0,
                &first as *const TestItimerspec as u64,
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
        "timer_settime should register its producer deadline in the unified timer registry"
    );
    delete_posix_timer(&ctx, timerid);
    assert_eq!(
        domain.next_deadline(),
        None,
        "timer_delete should cancel the POSIX timer deadline"
    );
}

#[test]
fn dispatch_posix_timer_poll_due_periodic_rearms_registry_deadline() {
    let (_setup, proc_cap, thread) = posix_timer_setup();
    let domain = Arc::new(DeadlineDomainTestDouble::default());
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(Arc::new(TaskMailbox::new()))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let timerid = create_posix_timer(&ctx);
    let periodic = TestItimerspec {
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
                NR_TIMER_SETTIME,
                [
                    timerid as u64,
                    0,
                    &periodic as *const TestItimerspec as u64,
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
        .expect("periodic POSIX timer should register first deadline");

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(domain.fire_due(first_deadline + 1), 1);
    assert_eq!(
        domain.next_deadline(),
        None,
        "manual fire removes the old POSIX timer registry entry before poll_due rearm"
    );

    let registrar = ctx.timer_registrar.as_ref().unwrap().clone();
    let timer_mailbox = ctx
        .mailbox
        .as_ref()
        .map(alloc::sync::Arc::downgrade)
        .expect("test context should carry a timer mailbox");
    let next_deadline = crate::linux_syscall::poll_due_posix_timers_with_post::<ShimsTestPmap, _>(
        &proc_cap,
        Some(&registrar),
        Some(timer_mailbox),
        |_mailbox, _event| {},
    );

    assert!(next_deadline.is_some());
    assert!(
        domain.next_deadline().is_some(),
        "periodic POSIX timer poll_due rearm should register the next deadline in the unified timer registry"
    );
    delete_posix_timer(&ctx, timerid);
    assert_eq!(
        domain.next_deadline(),
        None,
        "timer_delete should cancel the periodic POSIX timer deadline"
    );
}

#[test]
fn dispatch_posix_timer_signal_timer_hint_waits_for_due_scan() {
    let (_setup, proc_cap, thread) = posix_timer_setup();
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
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(Arc::clone(&mailbox))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let timerid = create_posix_timer(&ctx);
    let periodic = TestItimerspec {
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
                NR_TIMER_SETTIME,
                [
                    timerid as u64,
                    0,
                    &periodic as *const TestItimerspec as u64,
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
        .expect("POSIX timer should register first signal timer deadline");

    super::SHIMS_TEST_NS_COUNTER.store(first_deadline + 1, core::sync::atomic::Ordering::Release);
    assert_eq!(domain.fire_due(first_deadline + 1), 1);
    let mut signal_posts = 0usize;
    assert_eq!(
        mailbox.len(),
        1,
        "timer registry fire should enqueue a SignalTimerFired hint",
    );
    let signal_hint = mailbox
        .poll()
        .expect("timer registry fire should enqueue a signal timer hint");
    assert!(
        matches!(signal_hint, MailboxEvent::SignalTimerFired { .. }),
        "timer registry fire must enqueue SignalTimerFired, got {signal_hint:?}"
    );
    assert_eq!(
        signal_posts, 0,
        "SignalTimerFired is only a wake hint; it must not deliver the POSIX timer signal before due scan"
    );

    let registrar = ctx.timer_registrar.as_ref().unwrap().clone();
    let timer_mailbox = ctx
        .mailbox
        .as_ref()
        .map(alloc::sync::Arc::downgrade)
        .expect("test context should carry a timer mailbox");
    let next_deadline = crate::linux_syscall::poll_due_posix_timers_with_post::<ShimsTestPmap, _>(
        &proc_cap,
        Some(&registrar),
        Some(timer_mailbox),
        |_mailbox, _event| {
            signal_posts += 1;
        },
    );

    assert!(next_deadline.is_some());
    assert_eq!(
        signal_posts, 1,
        "due scan should deliver the POSIX timer signal through the normal signal post path"
    );
    delete_posix_timer(&ctx, timerid);
    assert_eq!(
        domain.next_deadline(),
        None,
        "timer_delete should cancel the periodic signal timer deadline"
    );
}
