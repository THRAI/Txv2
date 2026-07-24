#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod clock;
pub mod deadline;
pub mod driver;
pub mod hal;
pub mod keeper;
pub mod realtime;
pub mod rtc;
pub mod timer;
pub mod types;
pub mod vdso;

#[cfg(test)]
mod vdso_phase2_tests {
    use tx_hal::{MonotonicCounterIf, VdsoCounterInfo, VdsoCounterMode};

    use crate::{
        vdso::{
            counter_eligibility, VdsoClockMode, VvarData, VVAR_ABI_VERSION,
            VVAR_ABI_VERSION_OFFSET, VVAR_CLOCK_MODE_OFFSET, VVAR_CYCLE_LAST_OFFSET,
            VVAR_DATA_SIZE, VVAR_MASK_OFFSET, VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET,
            VVAR_MONOTONIC_SEC_OFFSET, VVAR_MULT_OFFSET, VVAR_REALTIME_GENERATION_OFFSET,
            VVAR_REALTIME_NSEC_SHIFTED_OFFSET, VVAR_REALTIME_SEC_OFFSET, VVAR_SEQ_OFFSET,
            VVAR_SHIFT_OFFSET,
        },
        VvarSnapshot,
    };

    struct CounterUnavailable;
    struct CounterUnstable;
    struct CounterKernelOnly;
    struct CounterReady;

    impl MonotonicCounterIf for CounterUnavailable {
        fn read_ns() -> u64 {
            0
        }

        fn frequency_hz() -> u64 {
            0
        }
    }

    impl MonotonicCounterIf for CounterUnstable {
        fn read_ns() -> u64 {
            0
        }

        fn frequency_hz() -> u64 {
            10_000_000
        }

        fn vdso_counter_info() -> Option<VdsoCounterInfo> {
            Some(VdsoCounterInfo {
                frequency_hz: 10_000_000,
                mask: !0,
                stable: false,
                user_readable: true,
                mode: VdsoCounterMode::RiscvTime,
            })
        }
    }

    impl MonotonicCounterIf for CounterKernelOnly {
        fn read_ns() -> u64 {
            0
        }

        fn frequency_hz() -> u64 {
            10_000_000
        }

        fn vdso_counter_info() -> Option<VdsoCounterInfo> {
            Some(VdsoCounterInfo {
                frequency_hz: 10_000_000,
                mask: !0,
                stable: true,
                user_readable: false,
                mode: VdsoCounterMode::RiscvTime,
            })
        }
    }

    impl MonotonicCounterIf for CounterReady {
        fn read_ns() -> u64 {
            0
        }

        fn frequency_hz() -> u64 {
            10_000_000
        }

        fn vdso_counter_info() -> Option<VdsoCounterInfo> {
            Some(VdsoCounterInfo {
                frequency_hz: 10_000_000,
                mask: !0,
                stable: true,
                user_readable: true,
                mode: VdsoCounterMode::RiscvTime,
            })
        }

        fn read_vdso_counter() -> u64 {
            77
        }
    }

    #[test]
    fn counter_eligibility_requires_available_stable_user_readable_counter() {
        assert!(counter_eligibility::<CounterUnavailable>().is_unavailable());
        assert!(counter_eligibility::<CounterUnstable>().is_unstable());
        assert!(counter_eligibility::<CounterKernelOnly>().is_kernel_only());

        let ready = counter_eligibility::<CounterReady>()
            .ready()
            .expect("stable user-readable counter is eligible");
        assert_eq!(ready.clock_mode(), VdsoClockMode::RiscvTime);
        assert_eq!(ready.calibration().mult(), 1_677_721_600);
        assert_eq!(ready.calibration().shift(), 24);
        assert_eq!(ready.read_counter::<CounterReady>(), 77);
    }

    #[test]
    fn vvar_data_has_one_page_v1_layout_and_maps_snapshot_fields() {
        let snapshot = VvarSnapshot {
            realtime_generation: 9,
            cycle_last: 8,
            mask: 7,
            mult: 6,
            shift: 5,
            realtime_sec: 4,
            realtime_nsec_shifted: 3,
            monotonic_sec: 2,
            monotonic_nsec_shifted: 1,
        };
        let data = VvarData::from_snapshot(snapshot, VdsoClockMode::RiscvTime);

        assert_eq!(VVAR_DATA_SIZE, 4096);
        assert_eq!(VVAR_SEQ_OFFSET, 0);
        assert_eq!(VVAR_ABI_VERSION_OFFSET, 8);
        assert_eq!(VVAR_CLOCK_MODE_OFFSET, 12);
        assert_eq!(VVAR_REALTIME_SEC_OFFSET, 16);
        assert_eq!(VVAR_REALTIME_NSEC_SHIFTED_OFFSET, 24);
        assert_eq!(VVAR_MONOTONIC_SEC_OFFSET, 32);
        assert_eq!(VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET, 40);
        assert_eq!(VVAR_CYCLE_LAST_OFFSET, 48);
        assert_eq!(VVAR_MULT_OFFSET, 56);
        assert_eq!(VVAR_SHIFT_OFFSET, 64);
        assert_eq!(VVAR_MASK_OFFSET, 72);
        assert_eq!(VVAR_REALTIME_GENERATION_OFFSET, 80);
        assert_eq!(data.abi_version, VVAR_ABI_VERSION);
        assert_eq!(data.clock_mode, VdsoClockMode::RiscvTime as u32);
        assert_eq!(data.realtime_generation, snapshot.realtime_generation);
        assert_eq!(data.cycle_last, snapshot.cycle_last);
    }
}

pub use clock::ClockRead;
pub use deadline::{
    DeadlineDomain, DeadlineRegistrar, DeadlineRegistrarHandle, DeviceTimerCallback, TimerGuard,
    TimerRole, TimerTarget, TimerToken,
};
pub use driver::CurrentHartDeadlineTimer;
#[cfg(any(test, feature = "test-support"))]
pub use keeper::reset_for_test;
pub use keeper::{
    install_realtime_timer_notifier, install_vvar_publish_hook, timekeeper, timekeeper_clock,
    RealtimeSeedError, RealtimeTimerNotifier, RealtimeWritebackPolicy, Timekeeper, TimekeeperClock,
    TimekeeperIf, VvarPublishHook, VvarSnapshot, WallClockError, DEFAULT_REALTIME_EPOCH_BASE_NS,
};
pub use realtime::{RealtimeControl, RealtimeSetPolicy, RealtimeSetReport, VvarPublisher};
pub use rtc::RtcDeviceOps;
pub use timer::TimerEngine;
pub use types::{ClockId, DeadlineNs, TimeError, TimerKey};

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use tx_hal::PersistentClockError;
    use tx_substrate::step::DelegateTokenId;
    use tx_substrate::wake::deadline::TimerGuardRole;

    struct RecordingDeadlineDomain {
        next_token: AtomicU64,
        registrations: std::sync::Mutex<Vec<(u64, TimerRole, bool)>>,
        cancellations: std::sync::Mutex<Vec<TimerToken>>,
        rearms: std::sync::Mutex<Vec<(TimerToken, u64)>>,
    }

    impl RecordingDeadlineDomain {
        fn new() -> Self {
            Self {
                next_token: AtomicU64::new(1),
                registrations: std::sync::Mutex::new(Vec::new()),
                cancellations: std::sync::Mutex::new(Vec::new()),
                rearms: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl DeadlineDomain for RecordingDeadlineDomain {
        fn register_deadline(
            &self,
            deadline_ns: DeadlineNs,
            role: TimerRole,
            target: TimerTarget,
        ) -> Result<TimerToken, TimeError> {
            self.registrations.lock().unwrap().push((
                deadline_ns.raw(),
                role,
                matches!(target, TimerTarget::DelegateToken(_)),
            ));
            Ok(TimerToken::new(
                self.next_token.fetch_add(1, Ordering::Relaxed),
            ))
        }

        fn cancel_deadline(&self, token: TimerToken) -> bool {
            self.cancellations.lock().unwrap().push(token);
            true
        }

        fn rearm_deadline(&self, token: TimerToken, deadline_ns: DeadlineNs) -> bool {
            self.rearms.lock().unwrap().push((token, deadline_ns.raw()));
            true
        }
    }

    struct TestClock {
        monotonic_ns: u64,
        realtime_ns: u64,
    }

    impl ClockRead for TestClock {
        fn monotonic_now_ns(&self) -> u64 {
            self.monotonic_ns
        }

        fn realtime_now_ns(&self) -> u64 {
            self.realtime_ns
        }
    }

    #[test]
    fn clock_read_now_ns_maps_supported_and_unsupported_clocks() {
        let clock = TestClock {
            monotonic_ns: 11,
            realtime_ns: 22,
        };

        assert_eq!(clock.now_ns(ClockId::Monotonic), Ok(11));
        assert_eq!(clock.now_ns(ClockId::Boottime), Ok(11));
        assert_eq!(clock.now_ns(ClockId::Realtime), Ok(22));
        assert_eq!(clock.now_ns(ClockId::Tai), Err(TimeError::Unsupported));
        assert_eq!(
            clock.now_ns(ClockId::ProcessCpuTime),
            Err(TimeError::Unsupported)
        );
        assert_eq!(
            clock.now_ns(ClockId::ThreadCpuTime),
            Err(TimeError::Unsupported)
        );
    }

    #[test]
    fn timer_roles_map_to_compatibility_guard_roles() {
        assert_eq!(
            TimerRole::PrimarySleep.guard_role(),
            TimerGuardRole::PrimarySleep
        );
        assert_eq!(
            TimerRole::DeadlineAbort.guard_role(),
            TimerGuardRole::DeadlineAbort
        );
        assert_eq!(
            TimerRole::DelegateTimeout.guard_role(),
            TimerGuardRole::DelegateTimeout
        );
        assert_eq!(
            TimerRole::DeviceEvent.guard_role(),
            TimerGuardRole::DeviceEvent
        );
        for role in [
            TimerRole::TimerFd,
            TimerRole::ItimerReal,
            TimerRole::PosixTimer,
            TimerRole::FutexTimeout,
            TimerRole::PollTimeout,
            TimerRole::RtcAlarm,
        ] {
            assert_eq!(role.guard_role(), TimerGuardRole::DeadlineAbort);
        }
    }

    #[test]
    fn deadline_guard_cancels_through_its_domain() {
        let domain = Arc::new(RecordingDeadlineDomain::new());
        let registrar = DeadlineRegistrarHandle::from_domain(domain.clone());

        let guard = registrar
            .register_deadline(
                DeadlineNs::new(17),
                TimerRole::TimerFd,
                TimerTarget::DelegateToken(DelegateTokenId::new(9)),
            )
            .unwrap();
        let token = guard.token();

        assert_eq!(
            *domain.registrations.lock().unwrap(),
            vec![(17, TimerRole::TimerFd, true)]
        );
        drop(guard);
        assert_eq!(*domain.cancellations.lock().unwrap(), vec![token]);
    }

    #[test]
    fn deadline_rearm_keeps_the_domain_registration_token() {
        let domain = Arc::new(RecordingDeadlineDomain::new());
        let registrar = DeadlineRegistrarHandle::from_domain(domain.clone());
        let mut guard = Some(
            registrar
                .register_deadline(
                    DeadlineNs::new(17),
                    TimerRole::PollTimeout,
                    TimerTarget::DelegateToken(DelegateTokenId::new(3)),
                )
                .unwrap(),
        );
        let token = guard.as_ref().unwrap().token();

        registrar
            .rearm_deadline(
                &mut guard,
                DeadlineNs::new(23),
                TimerRole::PollTimeout,
                TimerTarget::DelegateToken(DelegateTokenId::new(3)),
            )
            .unwrap();

        assert_eq!(guard.as_ref().unwrap().token(), token);
        assert_eq!(*domain.rearms.lock().unwrap(), vec![(token, 23)]);
        assert_eq!(domain.registrations.lock().unwrap().len(), 1);
    }

    #[test]
    fn persistent_clock_errors_convert_to_time_errors() {
        assert_eq!(
            TimeError::from(PersistentClockError::Unsupported),
            TimeError::Unsupported
        );
        assert_eq!(
            TimeError::from(PersistentClockError::Invalid),
            TimeError::Invalid
        );
        assert_eq!(
            TimeError::from(PersistentClockError::Range),
            TimeError::Range
        );
        assert_eq!(
            TimeError::from(PersistentClockError::Hardware),
            TimeError::Hardware
        );
    }
}
