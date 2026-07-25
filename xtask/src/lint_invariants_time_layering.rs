//! Hard lint for the time service layering migration.
//!
//! This gate turns the timer subsystem design's import/callsite boundaries into
//! a mechanical zero-finding boundary. Production code should reach time through
//! the `tx_services::time` facades, not raw HAL time traits or the retired
//! pre-Phase 7 timer surface.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

#[derive(Clone, Copy)]
struct Rule {
    name: &'static str,
    replacement: &'static str,
    scope: &'static [&'static str],
    needles: &'static [&'static str],
    allow: &'static [&'static str],
}

const COMMON_ALLOW: &[&str] = &["/tests/", "/tests.rs", "_tests.rs", "/testing.rs"];
const RETIRED_WAKE_TIMER_MODULE: &str = concat!("tx_substrate::wake::", "timer");
const RETIRED_TIMER_WHEEL: &str = concat!("Timer", "Wheel");
const RETIRED_TIMER_WAKE_ROUTER: &str = concat!("TimerWake", "Router");
const RETIRED_CURRENT_TIMER_WHEEL: &str = concat!("current_timer_", "wheel");
const RETIRED_SET_CURRENT_TIMER_WHEEL: &str = concat!("set_current_timer_", "wheel");
const RETIRED_REGISTRAR_LOWERING: &str =
    concat!("into_substrate_registrar_for_script_", "bri", "dge");

const RULES: &[Rule] = &[
    Rule {
        name: "raw HAL time trait or callsite",
        replacement: "Use tx_services::time::{ClockRead, RealtimeControl, ReactorTimeDriver, RtcDeviceOps}.",
        scope: &["crates/", "boards/"],
        needles: &[
            "tx_hal::MonotonicCounterIf",
            "tx_hal::DeadlineTimerIf",
            "tx_hal::PersistentClockIf",
            "MonotonicCounterIf",
            "DeadlineTimerIf",
            "PersistentClockIf",
            "P::read_ns(",
            "P::set_deadline_ns(",
            "P::cancel_deadline(",
            "P::acknowledge_wake_alarm_irq(",
            "PersistentClockIf::read_realtime_ns(",
            "PersistentClockIf::set_realtime_ns(",
            "PersistentClockIf::acknowledge_wake_alarm_irq(",
        ],
        allow: &[
            "crates/tx-hal/",
            "boards/",
            "crates/tx-services/src/time/platform.rs",
            "crates/tx-services/src/time/wall_clock.rs",
            "crates/tx-services/src/time/realtime.rs",
            "crates/tx-services/src/time/driver.rs",
            "crates/tx-time/",
            "crates/tx-observe/",
        ],
    },
    Rule {
        name: "retired pre-Phase 7 timer surface",
        replacement: "Use tx_substrate::wake::deadline::{TimerToken, TimerGuardRole} and the deadline-owned service interfaces.",
        scope: &["crates/", "boards/", "xtask/"],
        needles: &[
            RETIRED_WAKE_TIMER_MODULE,
            RETIRED_TIMER_WHEEL,
            RETIRED_TIMER_WAKE_ROUTER,
            RETIRED_CURRENT_TIMER_WHEEL,
            RETIRED_SET_CURRENT_TIMER_WHEEL,
            RETIRED_REGISTRAR_LOWERING,
        ],
        allow: &[],
    },
    Rule {
        name: "public timer implementation export",
        replacement: "Keep deadline implementation exports owned by tx_substrate::wake::deadline or tx_services::time.",
        scope: &[
            "crates/tx-reactor/src/lib.rs",
            "crates/tx-reactor/src/adapter.rs",
            "crates/tx-reactor/src/timer.rs",
            "crates/tx-kernel/src/adapter.rs",
            "crates/tx-shims/src/adapter.rs",
        ],
        needles: &[
            "pub mod timer",
            "pub use timer",
            "DeviceTimerCallback",
            "TimerRegistrar",
            "TimerRegistrarHandle",
            "TimerRegistry",
            RETIRED_TIMER_WAKE_ROUTER,
        ],
        allow: &[],
    },
    Rule {
        name: "broad subsystem wake adapter export",
        replacement: "Do not re-export tx_substrate::wake wholesale from tx-subsystems adapters; expose only the specific wait/mailbox/diagnostic names needed by that boundary.",
        scope: &["crates/tx-subsystems/src/adapter.rs"],
        needles: &["pub use tx_substrate::wake;"],
        allow: &[],
    },
    Rule {
        name: "legacy subsystem wall-clock facade import",
        replacement: "Use tx_services::time::{timekeeper, timekeeper_clock, TimekeeperClock, TimekeeperIf, DEFAULT_REALTIME_EPOCH_BASE_NS, reset_for_test}; use tx_subsystems::time_hooks only for subsystem hook installation.",
        scope: &["crates/", "boards/"],
        needles: &[
            "tx_subsystems::wall_clock",
            "tx_subsystems::wall_clock::timekeeper",
            "tx_subsystems::wall_clock::Timekeeper",
            "tx_subsystems::wall_clock::DEFAULT_REALTIME_EPOCH_BASE_NS",
            "crate::wall_clock",
            "crate::wall_clock::timekeeper",
            "crate::wall_clock::Timekeeper",
            "crate::wall_clock::DEFAULT_REALTIME_EPOCH_BASE_NS",
        ],
        allow: &[],
    },
    Rule {
        name: "semantic object storing time service handle",
        replacement: "Semantic objects should store TimerGuard/TimerToken, not Time service handles.",
        scope: &["crates/tx-subsystems/src/"],
        needles: &[
            "TimeService",
            "TimeServiceHandle",
            "ClockReadHandle",
            "RealtimeControlHandle",
            "DeadlineRegistrarHandle",
            "TimerRegistrarHandle",
        ],
        allow: &[
            "crates/tx-services/src/time/",
        ],
    },
];

pub(crate) fn lint_invariants_time_layering(root: &Path) -> Result<()> {
    let mut findings = Vec::new();
    let mut by_rule: BTreeMap<&'static str, usize> = BTreeMap::new();

    for root_rel in ["crates", "boards", "xtask"] {
        let path = root.join(root_rel);
        if !path.exists() {
            continue;
        }
        for file in collect_files(&path, &["rs"]).map_err(|err| err.to_string())? {
            let rel = relative(root, &file).replace('\\', "/");
            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            for (line_idx, line) in text.lines().enumerate() {
                let Some(code) = code_before_comment(line) else {
                    continue;
                };
                for rule in RULES {
                    if !rule.scope.iter().any(|prefix| rel.starts_with(prefix)) {
                        continue;
                    }
                    if is_allowed(&rel, rule.allow) {
                        continue;
                    }
                    if rule.needles.iter().any(|needle| code.contains(needle)) {
                        *by_rule.entry(rule.name).or_insert(0) += 1;
                        findings.push(format!(
                            "{rel}:{} - {}: {}",
                            line_idx + 1,
                            rule.name,
                            line.trim().chars().take(140).collect::<String>()
                        ));
                    }
                }
            }
        }
    }

    println!("Invariants Lint - time-layering");
    println!("================================");
    println!("time layering findings: {:>4}  (ceiling 0)", findings.len());
    if !by_rule.is_empty() {
        println!();
        println!("  by rule:");
        for (rule, count) in &by_rule {
            println!("  {rule:<48} {count:>4}");
        }
    }
    if !findings.is_empty() {
        println!();
        println!("  sites (first 80):");
        for finding in findings.iter().take(80) {
            println!("  {finding}");
        }
        if findings.len() > 80 {
            println!("  ... and {} more", findings.len() - 80);
        }
        println!();
        println!("  replacements:");
        for rule in RULES {
            println!("  - {}: {}", rule.name, rule.replacement);
        }
    }

    if findings.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "time-layering lint found {} issue(s); use the replacement interfaces above",
            findings.len()
        ))
    }
}

fn is_allowed(rel: &str, allow: &[&str]) -> bool {
    allow.iter().any(|prefix| rel.starts_with(prefix))
        || COMMON_ALLOW.iter().any(|part| rel.contains(part))
}

fn code_before_comment(line: &str) -> Option<&str> {
    let code = line
        .split_once("//")
        .map_or(line, |(before, _)| before)
        .trim();
    if code.is_empty() {
        None
    } else {
        Some(code)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn time_layering_lint_fails_on_raw_hal_time_in_production() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/bad/src");
        fs::create_dir_all(&crate_dir).expect("create temp crate dir");
        fs::write(
            crate_dir.join("lib.rs"),
            "pub fn bad<P>() { let _ = P::read_ns(); }\n",
        )
        .expect("write temp bad source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("raw HAL time use should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_retired_timer_surface_in_xtask() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-xtask-retired-timer-lint-{}-{unique}",
            std::process::id()
        ));
        let xtask_dir = root.join("xtask/src");
        fs::create_dir_all(&xtask_dir).expect("create temp xtask dir");
        fs::write(
            xtask_dir.join("retired.rs"),
            format!(
                "use {}::{};\n",
                super::RETIRED_WAKE_TIMER_MODULE,
                super::RETIRED_TIMER_WHEEL
            ),
        )
        .expect("write temp retired xtask source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("retired timer surface in xtask should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_legacy_wall_clock_facade_in_production() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-wall-clock-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/bad/src");
        fs::create_dir_all(&crate_dir).expect("create temp crate dir");
        fs::write(
            crate_dir.join("lib.rs"),
            "use tx_subsystems::wall_clock::{timekeeper_clock, TimekeeperClock};\n",
        )
        .expect("write temp bad source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("legacy wall-clock facade use should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_syscall_context_substrate_timer_handle() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-syscall-ctx-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-shims/src/linux_syscall");
        fs::create_dir_all(&crate_dir).expect("create temp syscall ctx dir");
        fs::write(
            crate_dir.join("ctx.rs"),
            format!(
                "use {}::TimerRegistrarHandle;\n",
                super::RETIRED_WAKE_TIMER_MODULE
            ),
        )
        .expect("write temp bad ctx source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("syscall ctx substrate timer handle should fail the hard lint");
        assert!(err.contains("time-layering lint found"));
    }

    #[test]
    fn time_layering_lint_fails_on_shims_adapter_timer_reexport() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-shims-adapter-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-shims/src");
        fs::create_dir_all(&crate_dir).expect("create temp shims adapter dir");
        fs::write(
            crate_dir.join("adapter.rs"),
            format!(
                "pub use {}::TimerRegistrarHandle;\n",
                super::RETIRED_WAKE_TIMER_MODULE
            ),
        )
        .expect("write temp bad adapter source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("shims adapter timer re-export should fail the hard lint");
        assert!(err.contains("time-layering lint found"));
    }

    #[test]
    fn time_layering_lint_allows_deadline_token_types() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-deadline-token-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-kernel/src");
        fs::create_dir_all(&crate_dir).expect("create temp kernel adapter dir");
        fs::write(
            crate_dir.join("adapter.rs"),
            "use tx_substrate::wake::deadline::{TimerGuardRole, TimerToken};\n",
        )
        .expect("write temp deadline token source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        result.expect("deadline TimerToken and TimerGuardRole should remain allowed");
    }

    #[test]
    fn time_layering_lint_fails_on_service_facade_substrate_callback_reexport() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-service-callback-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-services/src/time");
        fs::create_dir_all(&crate_dir).expect("create temp time service dir");
        fs::write(
            crate_dir.join("deadline.rs"),
            format!(
                "pub use {}::{{DeviceTimerCallback, TimerGuard}};\n",
                super::RETIRED_WAKE_TIMER_MODULE
            ),
        )
        .expect("write temp bad time service source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("service callback re-export should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_service_facade_substrate_guard_reexport() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-service-guard-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-services/src/time");
        fs::create_dir_all(&crate_dir).expect("create temp time service dir");
        fs::write(
            crate_dir.join("deadline.rs"),
            format!(
                "pub use {}::{{TimerGuard, TimerToken}};\n",
                super::RETIRED_WAKE_TIMER_MODULE
            ),
        )
        .expect("write temp bad time service source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("service guard re-export should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_upper_direct_substrate_timer_import() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-direct-substrate-timer-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-subsystems/src");
        fs::create_dir_all(&crate_dir).expect("create temp subsystem dir");
        fs::write(
            crate_dir.join("timer_leak.rs"),
            format!(
                "use {}::{};\n",
                super::RETIRED_WAKE_TIMER_MODULE,
                super::RETIRED_TIMER_WAKE_ROUTER
            ),
        )
        .expect("write temp bad subsystem source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("upper direct substrate timer import should fail");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_subsystems_adapter_broad_wake_reexport() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-subsystems-adapter-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/tx-subsystems/src");
        fs::create_dir_all(&crate_dir).expect("create temp subsystems adapter dir");
        fs::write(
            crate_dir.join("adapter.rs"),
            "pub use tx_substrate::wake;\n",
        )
        .expect("write temp bad adapter source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err =
            result.expect_err("subsystems adapter broad wake re-export should fail the hard lint");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn time_layering_lint_fails_on_retired_registrar_lowering() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-time-layering-retired-registrar-lowering-lint-{}-{unique}",
            std::process::id()
        ));
        let crate_dir = root.join("crates/bad/src");
        fs::create_dir_all(&crate_dir).expect("create temp bad crate dir");
        fs::write(
            crate_dir.join("lib.rs"),
            format!(
                "pub fn bad(registrar: tx_services::time::DeadlineRegistrarHandle) {{ let _ = registrar.{}(); }}\n",
                super::RETIRED_REGISTRAR_LOWERING
            ),
        )
        .expect("write temp bad source");

        let result = super::lint_invariants_time_layering(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        let err = result.expect_err("retired registrar lowering should fail");
        assert!(err.contains("time-layering lint found 1 issue"));
    }

    #[test]
    fn subsystems_no_longer_exports_legacy_wall_clock_module() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let lib_rs = fs::read_to_string(root.join("crates/tx-subsystems/src/lib.rs"))
            .expect("read tx-subsystems lib.rs");
        assert!(
            !lib_rs.contains("pub mod wall_clock"),
            "tx-subsystems must not re-export the retired wall_clock compatibility module"
        );
        assert!(
            !root.join("crates/tx-subsystems/src/wall_clock.rs").exists(),
            "retired tx-subsystems wall_clock shim file must not be restored"
        );
        assert!(
            lib_rs.contains("pub mod time_hooks"),
            "subsystem-local time hooks should be named by hook wiring, not wall-clock ownership"
        );
    }

    #[test]
    fn io_syscall_timeout_body_uses_time_facade_for_monotonic_reads() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let io_rs = root.join("crates/tx-shims/src/linux_syscall/io.rs");
        let text = fs::read_to_string(&io_rs).expect("read io.rs");

        for raw_read in [
            "P::read_ns()",
            "<P as tx_hal::MonotonicCounterIf>::read_ns()",
        ] {
            assert!(
                !text.contains(raw_read),
                "io.rs timeout/readiness bodies must use tx_services::time::ClockRead instead of raw monotonic read {raw_read}"
            );
        }
    }

    #[test]
    fn ipc_and_signal_wait_timeout_bodies_use_time_facade_for_monotonic_reads() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-shims/src/linux_syscall/ipc.rs",
            "crates/tx-shims/src/linux_syscall/signal.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            for raw_read in [
                "P::read_ns()",
                "<P as tx_hal::MonotonicCounterIf>::read_ns()",
            ] {
                assert!(
                    !text.contains(raw_read),
                    "{rel} wait-timeout bodies must use tx_services::time::ClockRead instead of raw monotonic read {raw_read}"
                );
            }
        }
    }

    #[test]
    fn kernel_boot_reactor_bodies_use_time_facade_for_monotonic_reads() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-kernel/src/init.rs",
            "crates/tx-kernel/src/init/exec.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            assert!(
                !text.contains("P::read_ns()"),
                "{rel} boot/reactor bodies must use tx_services::time::ClockRead instead of raw monotonic read P::read_ns()"
            );
            assert!(
                !text.contains("<P as tx_hal::MonotonicCounterIf>::read_ns"),
                "{rel} boot/reactor bodies must use tx_services::time::ClockRead instead of raw monotonic read function pointer"
            );
        }
    }

    #[test]
    fn kernel_boot_reactor_deadline_programming_uses_time_platform_adapter() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-kernel/src/init.rs",
            "crates/tx-kernel/src/init/exec.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            for raw_call in [
                "P::set_deadline_ns(",
                "P::cancel_deadline(",
                "P::enable_timer_wakeups(",
            ] {
                assert!(
                    !text.contains(raw_call),
                    "{rel} boot/reactor deadline programming must use tx_services::time::platform::HalDeadlineTimer instead of raw HAL call {raw_call}"
                );
            }
        }
    }

    #[test]
    fn thread_future_and_trap_deadline_programming_uses_time_platform_adapter() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for (rel, raw_calls) in [
            (
                "crates/tx-kernel/src/thread_future.rs",
                &["P::set_deadline_ns("][..],
            ),
            ("crates/tx-kernel/src/trap.rs", &["P::cancel_deadline("][..]),
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            for raw_call in raw_calls {
                assert!(
                    !text.contains(raw_call),
                    "{rel} runtime deadline programming must use tx_services::time::platform::HalDeadlineTimer instead of raw HAL call {raw_call}"
                );
            }
        }
    }

    #[test]
    fn signal_wait_paths_use_registrar_backed_deadlines() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let rel = "crates/tx-shims/src/linux_syscall/signal.rs";
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
            panic!("read {rel}: {err}");
        });
        assert!(
            !text.contains("P::set_deadline_ns("),
            "{rel} wait paths must use registrar-backed waits instead of raw HAL deadline programming"
        );
    }

    #[test]
    fn syscall_timer_producers_use_registrar_backed_deadlines() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-shims/src/linux_syscall/time.rs",
            "crates/tx-shims/src/linux_syscall/posix_timer.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            assert!(
                !text.contains("P::set_deadline_ns("),
                "{rel} timer producer paths must register deadlines through tx_services::time::DeadlineRegistrar instead of raw HAL deadline programming"
            );
        }
    }

    #[test]
    fn signal_timer_producers_register_signal_targets() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-shims/src/linux_syscall/time.rs",
            "crates/tx-shims/src/linux_syscall/posix_timer.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            assert!(
                text.contains("TimerTarget::SignalTarget"),
                "{rel} POSIX/itimer producer paths must register signal timer deadlines with TimerTarget::SignalTarget"
            );
        }
    }

    #[test]
    fn reactor_runtime_and_task_do_not_expose_retired_deadline_implementation() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-reactor/src/runtime.rs",
            "crates/tx-reactor/src/task.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            for forbidden in [
                super::RETIRED_TIMER_WHEEL,
                super::RETIRED_CURRENT_TIMER_WHEEL,
                super::RETIRED_SET_CURRENT_TIMER_WHEEL,
            ] {
                assert!(
                    !text.contains(forbidden),
                    "{rel} must route through ReactorDeadlineRegistry and DeadlineRegistrarHandle, not concrete {forbidden}"
                );
            }
        }
    }

    #[test]
    fn public_api_does_not_reexport_timer_implementation_surface() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-reactor/src/lib.rs",
            "crates/tx-reactor/src/adapter.rs",
            "crates/tx-shims/src/adapter.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            for forbidden in [
                "pub mod timer",
                "pub use timer",
                "DeviceTimerCallback",
                "TimerRegistrar",
                "TimerRegistrarHandle",
                "TimerRegistry",
                super::RETIRED_TIMER_WAKE_ROUTER,
            ] {
                assert!(
                    !text.contains(forbidden),
                    "{rel} must not re-export substrate timer implementation surface {forbidden}"
                );
            }
        }
    }

    #[test]
    fn semantic_timer_objects_do_not_store_substrate_timer_handles() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in [
            "crates/tx-subsystems/src/device.rs",
            "crates/tx-shims/src/linux_syscall/ctx.rs",
            "crates/tx-shims/src/linux_syscall/wait.rs",
            "crates/tx-shims/src/linux_syscall/time.rs",
            "crates/tx-shims/src/linux_syscall/posix_timer.rs",
        ] {
            let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
                panic!("read {rel}: {err}");
            });
            assert!(
                !text.contains("TimerRegistrarHandle"),
                "{rel} must carry timer registration through tx_services::time facade names, not substrate TimerRegistrarHandle"
            );
        }
    }

    #[test]
    fn syscall_ctx_has_no_retired_registrar_lowering() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let rel = "crates/tx-shims/src/linux_syscall/ctx.rs";
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
            panic!("read {rel}: {err}");
        });
        assert!(
            !text.contains(super::RETIRED_REGISTRAR_LOWERING),
            "{rel} must not restore the retired registrar lowering"
        );
    }

    #[test]
    fn deadline_module_exposes_phase_7_token_types() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let rel = "crates/tx-substrate/src/wake/deadline.rs";
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|err| {
            panic!("read {rel}: {err}");
        });
        assert!(
            text.contains("TimerToken") && text.contains("TimerGuardRole"),
            "{rel} must retain the Phase 7 token types"
        );
        assert!(
            !text.contains(super::RETIRED_WAKE_TIMER_MODULE),
            "{rel} must not restore the retired timer module"
        );
    }
}
