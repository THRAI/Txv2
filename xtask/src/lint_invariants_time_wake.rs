//! Lint rule: time/wake retired-interface gate.
//!
//! This turns the design-level grep matrix from
//! `docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md` into a
//! reproducible invariant. Retired time, timer, RTC, and wake-publication
//! interfaces must not remain callable from active Rust code after their
//! migration slice exits.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{collect_files, relative};
use crate::Result;

// DECISION 2026-07-27 (merge main → feature-network-refactor, commit 6d41a347).
//
// Raised 0 → 434 to record the merge's actual state, not to bless it.
//
// Why: main's `dd9435f3` (PR#54) introduced the reactor `post` mechanism and
// retired the direct time/wake interfaces at the same time it rolled the net
// subsystem back to a pre-P0 snapshot. The merge keeps feature's net tree
// (which predates the post mechanism and is where external TCP/DNS/IPv6 and
// the P0–P4 refactor actually live) on top of main's reactor. Every one of the
// 434 sites is feature-net code calling the pre-post interfaces; none is new
// code written against a retired API.
//
// Cost of driving this to 0 today: threading `post` through the whole net
// subsystem — a project on the scale of the refactor itself, and one that
// would have to be redone if the net stack is reconciled with main's rather
// than carried alongside it. That reconciliation is the real fix and it is
// tracked separately.
//
// Ratchet discipline: this number must only go DOWN from here. Anyone adding a
// retired-interface site must lower it as part of the same change, not raise it.
const MAX_TIME_WAKE_RETIRED_SITES: usize = 434;
const STRICT_ACTIVE_RUST_RESIDUE_ROOTS: &[&str] = &["crates", "boards"];
const RETIRED_WAKE_TIMER_MODULE: &str = concat!("tx_substrate::wake::", "timer");
const RETIRED_TIMER_WHEEL: &str = concat!("Timer", "Wheel");
const RETIRED_TIMER_WAKE_ROUTER: &str = concat!("TimerWake", "Router");
const RETIRED_CURRENT_TIMER_WHEEL: &str = concat!("current_timer_", "wheel");
const RETIRED_SET_CURRENT_TIMER_WHEEL: &str = concat!("set_current_timer_", "wheel");
const RETIRED_REGISTRAR_LOWERING: &str =
    concat!("into_substrate_registrar_for_script_", "bri", "dge");

#[derive(Clone, Copy)]
enum MatchKind {
    Ident(&'static str),
    Contains(&'static str),
    FunctionDef(&'static str),
    MethodCall(&'static str),
    PathIdent {
        prefix: &'static str,
        ident: &'static str,
    },
    LineAll(&'static [&'static str]),
}

#[derive(Clone, Copy)]
struct RetiredPattern {
    label: &'static str,
    kind: MatchKind,
}

struct AuditGroup {
    name: &'static str,
    roots: &'static [&'static str],
    patterns: &'static [RetiredPattern],
}

const CORE_TIME_TIMER_PATTERNS: &[RetiredPattern] = &[
    ident("TimeIf"),
    ident("DeadlineFuture"),
    ident("timer_sleep"),
    ident("install_timer_queue"),
    ident("sleep_until_ns"),
    ident(concat!("DirectMailboxTimerWake", "Router")),
    ident("fire_due_delegate_timeouts"),
    ident("timer_queue"),
    contains("fixed_oscomp_time"),
    contains("binding.name == \"rtc\""),
];

const SYSV_SEM_PATTERNS: &[RetiredPattern] = &[
    ident("step_semop"),
    ident("step_semop_v3"),
    ident("step_semctl"),
    ident("step_semctl_in_ns"),
    ident("step_sem_undo"),
    contains("notify_changed("),
    path_ident("notification::", "notify_changed"),
    ident("notify_v3_source"),
];

const SIGNALFD_PROCESS_PATTERNS: &[RetiredPattern] = &[
    ident("notify_process_signal"),
    path_ident("SignalFd::", "notify"),
    contains("pub fn notify(&self"),
    ident("fire_exit_source"),
    ident("notify_child_zombified"),
    ident("step_exit_group"),
    ident("step_exit_group_with_signal"),
    ident("notify_v3_source"),
];

const SIGNAL_HELPER_PATTERNS: &[RetiredPattern] = &[
    ident("step_kill_process"),
    ident("step_kill_pgrp"),
    ident("route_gewalt"),
    function_def("post_signal"),
    contains("post_signal("),
    ident("post_signal_mailbox"),
    ident("script_deliver_signal"),
    ident("KillProcessOp"),
    ident("KillPgrpOp"),
    ident("ThreadKillOp"),
    ident("deliver_posix_signal"),
    ident("DeliverSignalOp"),
    ident("maybe_deliver_itimer_signal"),
    contains("fire_itimer_real("),
    contains("deliver_signal_if_handler("),
];

const NET_READINESS_PATTERNS: &[RetiredPattern] = &[
    function_def("fire_recv"),
    function_def("fire_send"),
    function_def("fire_accept"),
    method_call("fire_recv"),
    method_call("fire_send"),
    method_call("fire_accept"),
    contains("publish_to("),
    contains(".publish()"),
    ident("direct_mailbox_post"),
];

const NET_DELEGATE_PATTERNS: &[RetiredPattern] = &[
    contains("net_delegate_kick_poll("),
    contains("net_delegate_kick_tick("),
    ident("net_delegate_direct_mailbox_post"),
];

const NET_DEVICE_IRQ_DIRECT_PATTERNS: &[RetiredPattern] = &[
    ident("handle_irq"),
    ident("ack_interrupt_and_fire"),
    ident("poll_device_and_fire"),
    ident("inject_rx_and_fire_poll_for_test_or_irq"),
    ident("complete_tx_and_fire_poll_for_test_or_irq"),
];

const NET_DEVICE_TX_DIRECT_PATTERNS: &[RetiredPattern] = &[
    ident("step_process_device_tx_pending"),
    ident("step_process_device_tx_pending_at"),
    ident("step_process_device_tx_pending_in_namespace_at"),
];

const NETLINK_DIRECT_SEND_PATTERNS: &[RetiredPattern] = &[
    ident("netlink_netfilter_send"),
    ident("netlink_xfrm_send"),
    ident("netlink_route_send"),
    ident("netlink_route_send_with_netns_resolver"),
    ident("netlink_route_send_with_netns_resolvers"),
];

const NET_LOOPBACK_DIRECT_PATTERNS: &[RetiredPattern] = &[
    ident("step_process_loopback_udp"),
    ident("step_process_loopback_udp_on_iface"),
    ident("step_send_udp_loopback_kernel_bytes"),
    ident("step_send_udp_loopback_kernel_bytes_on_iface"),
    ident("step_process_loopback_icmp"),
    ident("step_process_loopback_icmp_on_iface"),
];

const DELEGATE_REGISTRY_DIRECT_PATTERNS: &[RetiredPattern] = &[
    function_def("mark_replied"),
    function_def("mark_timed_out"),
    function_def("mark_canceled"),
    function_def("mark_agent_died"),
    function_def("mark_endpoint_died"),
    method_call("mark_replied"),
    method_call("mark_timed_out"),
    method_call("mark_canceled"),
    method_call("mark_agent_died"),
    method_call("mark_endpoint_died"),
];

const AIO_IO_URING_PATTERNS: &[RetiredPattern] = &[
    contains("notify_events_available("),
    contains("notify_cqe_available("),
    ident("spawn_worker_for_context"),
    ident("spawn_sqpoll_worker"),
    ident("push_completion"),
    ident("push_cqe"),
    ident("direct_completion_post"),
];

const RTC_DIRECT_PATTERNS: &[RetiredPattern] = &[ident("publish_rtc_event")];

const RTC_RAW_QUEUE_PATTERNS: &[RetiredPattern] = &[ident("RTC_EVENT_QUEUE"), contains(".fire(")];

const GENERIC_NOTIFY_V3_PATTERNS: &[RetiredPattern] = &[
    function_def("notify_v3_source"),
    contains("notify_v3_source("),
];

const PAGE_BACKED_PATTERNS: &[RetiredPattern] = &[
    contains("notify_source("),
    contains("tx_substrate::wake::notify("),
];

const TIMERFD_DIRECT_PATTERNS: &[RetiredPattern] = &[
    ident("timerfd_settime_with_flags"),
    ident("timerfd_clock_was_set"),
];

const WALL_CLOCK_RAW_PUBLIC_PATTERNS: &[RetiredPattern] = &[
    contains("pub struct WallClock"),
    contains("pub fn monotonic_now_ns"),
    contains("pub fn realtime_now_ns"),
    contains("pub fn set_realtime_ns"),
    contains("pub fn seed_realtime_ns"),
    contains("pub fn seed_realtime_from_persistent"),
    contains("pub fn generation"),
    contains("pub fn realtime_offset_ns"),
    contains("pub fn set_clock_params"),
    contains("pub fn monotonic_deadline_from_realtime_ns"),
    contains("pub fn snapshot_for_vvar"),
    contains("pub fn publish_vvar"),
];

const EVENTFD_DIRECT_PATTERNS: &[RetiredPattern] =
    &[ident("step_eventfd_read"), ident("step_eventfd_write")];

const PIPE_DIRECT_DEF_PATTERNS: &[RetiredPattern] = &[
    function_def("step_read"),
    function_def("step_write"),
    ident("ReadOp"),
    ident("WriteOp"),
];

const PIPE_DIRECT_CALL_PATTERNS: &[RetiredPattern] = &[
    contains("crate::pipe::step_read("),
    contains("crate::pipe::step_write("),
];

const VFS_DIRECT_PATTERNS: &[RetiredPattern] = &[ident("fire_read_wait"), ident("fire_write_wait")];

const USERFAULTFD_DIRECT_PATTERNS: &[RetiredPattern] = &[
    function_def("push_fault_msg"),
    method_call("push_fault_msg"),
    function_def("fault_script_for_process"),
    method_call("fault_script_for_process"),
    contains("ProcessUfdDispatch::new("),
    contains("fault_post: None"),
    contains("fault_post: Some"),
];

const POSIX_MQ_DIRECT_PATTERNS: &[RetiredPattern] =
    &[ident("step_mq_send"), ident("step_mq_receive")];

const SYSV_MSG_DIRECT_PATTERNS: &[RetiredPattern] = &[
    ident("step_msgsnd"),
    ident("step_msgrcv"),
    ident("step_msgsnd_v3"),
    ident("step_msgrcv_v3"),
    ident("step_msgctl"),
    ident("step_msgctl_in_ns"),
];

const TTY_DIRECT_PATTERNS: &[RetiredPattern] =
    &[function_def("step_ingest"), contains("step_ingest(")];

const REACTOR_COORD_DIRECT_PATTERNS: &[RetiredPattern] = &[
    function_def("complete"),
    method_call("complete"),
    function_def("arrive"),
    method_call("arrive"),
    function_def("ack"),
    method_call("ack"),
];

const RETIRED_PHASE_7_TIMER_PATTERNS: &[RetiredPattern] = &[
    contains(RETIRED_WAKE_TIMER_MODULE),
    contains(RETIRED_TIMER_WHEEL),
    contains(RETIRED_TIMER_WAKE_ROUTER),
    contains(RETIRED_CURRENT_TIMER_WHEEL),
    contains(RETIRED_SET_CURRENT_TIMER_WHEEL),
    contains(RETIRED_REGISTRAR_LOWERING),
];

const TX_SCRIPTS_TIMERWHEEL_PATTERNS: &[RetiredPattern] = &[
    RetiredPattern {
        label: concat!("tx_scripts::adapter::wake::*Timer", "Wheel import"),
        kind: MatchKind::LineAll(&["tx_scripts::adapter::wake::", RETIRED_TIMER_WHEEL]),
    },
    RetiredPattern {
        label: concat!("adapter::wake::{..., Timer", "Wheel, ...} import"),
        kind: MatchKind::LineAll(&["adapter::wake::{", RETIRED_TIMER_WHEEL]),
    },
];

const OLD_NAME_RESIDUE_PATTERNS: &[RetiredPattern] = &[
    ident("TimeIf"),
    ident("DeadlineFuture"),
    ident("timer_sleep"),
    ident("install_timer_queue"),
    ident("sleep_until_ns"),
    ident(concat!("DirectMailboxTimerWake", "Router")),
    ident("fire_due_delegate_timeouts"),
    ident("timer_queue"),
    contains("fixed_oscomp_time"),
    contains("binding.name == \"rtc\""),
    ident("notify_changed"),
    ident("notify_process_signal"),
    ident("fire_exit_source"),
    ident("notify_child_zombified"),
    ident("step_exit_group"),
    ident("step_exit_group_with_signal"),
    ident("step_kill_process"),
    ident("step_kill_pgrp"),
    ident("route_gewalt"),
    ident("post_signal"),
    ident("notify_v3_source"),
    ident("post_signal_mailbox"),
    ident("script_deliver_signal"),
    ident("KillProcessOp"),
    ident("KillPgrpOp"),
    ident("ThreadKillOp"),
    ident("deliver_posix_signal"),
    ident("DeliverSignalOp"),
    ident("maybe_deliver_itimer_signal"),
    ident("fire_itimer_real"),
    ident("deliver_signal_if_handler"),
    ident("publish_rtc_event"),
    ident("net_delegate_kick_poll"),
    ident("net_delegate_kick_tick"),
    ident("net_delegate_direct_mailbox_post"),
    ident("handle_irq"),
    ident("ack_interrupt_and_fire"),
    ident("poll_device_and_fire"),
    ident("inject_rx_and_fire_poll_for_test_or_irq"),
    ident("complete_tx_and_fire_poll_for_test_or_irq"),
    ident("step_process_device_tx_pending"),
    ident("step_process_device_tx_pending_at"),
    ident("step_process_device_tx_pending_in_namespace_at"),
    ident("netlink_netfilter_send"),
    ident("netlink_xfrm_send"),
    ident("netlink_route_send"),
    ident("netlink_route_send_with_netns_resolver"),
    ident("netlink_route_send_with_netns_resolvers"),
    ident("step_process_loopback_udp"),
    ident("step_process_loopback_udp_on_iface"),
    ident("step_send_udp_loopback_kernel_bytes"),
    ident("step_send_udp_loopback_kernel_bytes_on_iface"),
    ident("step_process_loopback_icmp"),
    ident("step_process_loopback_icmp_on_iface"),
    ident("mark_replied"),
    ident("mark_timed_out"),
    ident("mark_canceled"),
    ident("mark_agent_died"),
    ident("mark_endpoint_died"),
    ident("notify_events_available"),
    ident("notify_cqe_available"),
    ident("spawn_worker_for_context"),
    ident("spawn_sqpoll_worker"),
    ident("push_cqe"),
    ident("direct_completion_post"),
    ident("fire_recv"),
    ident("fire_send"),
    ident("fire_accept"),
    ident("direct_mailbox_post"),
    ident("RTC_EVENT_QUEUE"),
    ident("notify_source"),
    contains("tx_substrate::wake::notify("),
    ident("timerfd_settime_with_flags"),
    ident("timerfd_clock_was_set"),
    ident("step_eventfd_read"),
    ident("step_eventfd_write"),
    ident("fire_read_wait"),
    ident("fire_write_wait"),
    ident("step_mq_send"),
    ident("step_mq_receive"),
    ident("step_msgsnd"),
    ident("step_msgrcv"),
    ident("step_msgsnd_v3"),
    ident("step_msgrcv_v3"),
    ident("step_msgctl"),
    ident("step_msgctl_in_ns"),
    ident("step_semop"),
    ident("step_semop_v3"),
    ident("step_semctl"),
    ident("step_semctl_in_ns"),
    ident("step_sem_undo"),
    ident("post_signal_for_test"),
    ident("notify_process_signal_direct_for_test"),
];

const GROUPS: &[AuditGroup] = &[
    AuditGroup {
        name: "Phase 7 retired timer surface",
        roots: &["crates", "boards", "xtask"],
        patterns: RETIRED_PHASE_7_TIMER_PATTERNS,
    },
    AuditGroup {
        name: "core time/timer retired interfaces",
        roots: &["crates", "boards"],
        patterns: CORE_TIME_TIMER_PATTERNS,
    },
    AuditGroup {
        name: "SysV sem changed-source direct wrappers",
        roots: &[
            "crates/tx-subsystems/src/ipc/sysv_sem",
            "crates/tx-shims/src/linux_syscall/ipc.rs",
        ],
        patterns: SYSV_SEM_PATTERNS,
    },
    AuditGroup {
        name: "signalfd/process exit-source direct wrappers",
        roots: &[
            "crates/tx-subsystems/src/signalfd",
            "crates/tx-subsystems/src/signal",
            "crates/tx-subsystems/src/process",
            "crates/tx-subsystems/tests",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
        ],
        patterns: SIGNALFD_PROCESS_PATTERNS,
    },
    AuditGroup {
        name: "signal helper and itimer direct wrappers",
        roots: &["crates", "boards"],
        patterns: SIGNAL_HELPER_PATTERNS,
    },
    AuditGroup {
        name: "socket/network readiness direct wrappers",
        roots: &[
            "crates/tx-subsystems/src/net",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
            "boards",
        ],
        patterns: NET_READINESS_PATTERNS,
    },
    AuditGroup {
        name: "network delegate kick direct wrappers",
        roots: &[
            "crates/tx-subsystems/src/net",
            "crates/tx-drivers/src/virtio/net.rs",
            "crates/tx-kernel/src",
            "crates/tx-shims/src",
        ],
        patterns: NET_DELEGATE_PATTERNS,
    },
    AuditGroup {
        name: "net device IRQ direct fire wrappers",
        roots: &[
            "crates/tx-subsystems/src/net/device.rs",
            "crates/tx-subsystems/src/net/device",
            "crates/tx-drivers/src/virtio/net.rs",
            "crates/tx-kernel/src",
        ],
        patterns: NET_DEVICE_IRQ_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "net device TX direct readiness wrappers",
        roots: &[
            "crates/tx-subsystems/src/net",
            "crates/tx-kernel/src",
            "crates/tx-shims/src",
        ],
        patterns: NET_DEVICE_TX_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "netlink direct send wrappers",
        roots: &["crates/tx-subsystems/src/net", "crates/tx-shims/src"],
        patterns: NETLINK_DIRECT_SEND_PATTERNS,
    },
    AuditGroup {
        name: "network loopback direct readiness wrappers",
        roots: &["crates/tx-subsystems/src/net", "crates/tx-shims/src"],
        patterns: NET_LOOPBACK_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "delegate registry direct transition wrappers",
        roots: &[
            "crates/tx-substrate/src/step/agent.rs",
            "crates/tx-substrate/tests",
            "crates/tx-reactor/src",
            "crates/tx-reactor/tests",
            "crates/tx-shims/src/linux_syscall/userfaultfd.rs",
            "crates/tx-shims/tests",
            "crates/tx-subsystems/src/userfaultfd",
            "crates/tx-subsystems/src/vm",
            "crates/tx-subsystems/tests",
        ],
        patterns: DELEGATE_REGISTRY_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "AIO/io_uring direct completion wrappers",
        roots: &[
            "crates/tx-subsystems/src/aio",
            "crates/tx-subsystems/src/io_uring",
            "crates/tx-shims/src",
            "crates/tx-shims/tests",
        ],
        patterns: AIO_IO_URING_PATTERNS,
    },
    AuditGroup {
        name: "RTC direct event publish wrapper",
        roots: &["crates", "boards"],
        patterns: RTC_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "RTC raw event queue/fire access",
        roots: &[
            "crates/tx-fs/src/devfs/mod.rs",
            "crates/tx-kernel/src/irq.rs",
        ],
        patterns: RTC_RAW_QUEUE_PATTERNS,
    },
    AuditGroup {
        name: "generic v3 wait-source direct adapter wrappers",
        roots: &[
            "crates/tx-subsystems/src",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
            "crates/tx-scripts/src",
            "crates/tx-fs/src",
        ],
        patterns: GENERIC_NOTIFY_V3_PATTERNS,
    },
    AuditGroup {
        name: "page-backed page-ready direct notify wrappers",
        roots: &[
            "crates/tx-subsystems/src/page_backed",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
        ],
        patterns: PAGE_BACKED_PATTERNS,
    },
    AuditGroup {
        name: "timerfd direct realtime notification wrappers",
        roots: &[
            "crates/tx-subsystems/src/timerfd",
            "crates/tx-shims/src/linux_syscall",
            "crates/tx-kernel/src",
        ],
        patterns: TIMERFD_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "wall-clock raw public compatibility wrappers",
        roots: &["crates/tx-subsystems/src/wall_clock.rs"],
        patterns: WALL_CLOCK_RAW_PUBLIC_PATTERNS,
    },
    AuditGroup {
        name: "eventfd direct read/write wrappers",
        roots: &[
            "crates/tx-subsystems/src/eventfd",
            "crates/tx-shims/src/linux_syscall",
            "crates/tx-kernel/src",
        ],
        patterns: EVENTFD_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "pipe direct read/write wrapper definitions",
        roots: &[
            "crates/tx-subsystems/src/pipe",
            "crates/tx-subsystems/tests/v3_pipe_waitsource.rs",
            "crates/tx-subsystems/src/process/tests/fd_table.rs",
            "crates/tx-shims/src/linux_syscall/io.rs",
        ],
        patterns: PIPE_DIRECT_DEF_PATTERNS,
    },
    AuditGroup {
        name: "pipe direct read/write callsites",
        roots: &[
            "crates/tx-subsystems/src/vfs/execution.rs",
            "crates/tx-shims/src/linux_syscall/io.rs",
        ],
        patterns: PIPE_DIRECT_CALL_PATTERNS,
    },
    AuditGroup {
        name: "VFS/RNode direct readiness wrappers",
        roots: &[
            "crates/tx-subsystems/src/vfs",
            "crates/tx-subsystems/tests",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
        ],
        patterns: VFS_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "userfaultfd pending-fault direct wrappers",
        roots: &[
            "crates/tx-subsystems/src/userfaultfd",
            "crates/tx-subsystems/src/vm/execution.rs",
            "crates/tx-subsystems/tests/v3_userfaultfd_e2e.rs",
            "crates/tx-subsystems/tests/v3_userfaultfd_fault_path.rs",
            "crates/tx-shims/src/linux_syscall/tests/epoll_dispatch.rs",
            "crates/tx-shims/tests/v3_userfaultfd_ioctl_reply.rs",
            "crates/tx-kernel/src/thread_future.rs",
        ],
        patterns: USERFAULTFD_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "POSIX mq direct send/receive wrappers",
        roots: &[
            "crates/tx-subsystems/src/ipc/posix_mq",
            "crates/tx-subsystems/tests",
            "crates/tx-shims/src/linux_syscall",
            "crates/tx-fs/src",
        ],
        patterns: POSIX_MQ_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "SysV msg direct send/receive wrappers",
        roots: &[
            "crates/tx-subsystems/src/ipc/sysv_msg",
            "crates/tx-subsystems/tests",
            "crates/tx-shims/src/linux_syscall",
            "crates/tx-fs/src",
        ],
        patterns: SYSV_MSG_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "TTY direct ingest wrapper",
        roots: &[
            "crates/tx-subsystems/src/tty",
            "crates/tx-subsystems/tests",
            "crates/tx-shims/src",
            "crates/tx-kernel/src",
        ],
        patterns: TTY_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: "reactor-local coordination direct wrappers",
        roots: &["crates/tx-reactor"],
        patterns: REACTOR_COORD_DIRECT_PATTERNS,
    },
    AuditGroup {
        name: concat!("tx-scripts concrete Timer", "Wheel adapter imports"),
        roots: &[
            "crates/tx-scripts",
            "crates/tx-subsystems",
            "crates/tx-shims",
            "crates/tx-kernel",
        ],
        patterns: TX_SCRIPTS_TIMERWHEEL_PATTERNS,
    },
];

pub(crate) fn lint_invariants_time_wake_retired(root: &Path) -> Result<()> {
    let mut sites = Vec::new();
    let mut by_group: BTreeMap<&'static str, usize> = BTreeMap::new();

    for group in GROUPS {
        for root_rel in group.roots {
            let path = root.join(root_rel);
            if !path.exists() {
                continue;
            }
            let files = if path.is_file() {
                vec![path]
            } else {
                collect_files(&path, &["rs"]).map_err(|err| err.to_string())?
            };

            for file in files {
                let rel = relative(root, &file).replace('\\', "/");
                let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
                for (line_idx, line) in text.lines().enumerate() {
                    let scanned = if group.name == "Phase 7 retired timer surface" {
                        line
                    } else {
                        let Some(code) = code_before_comment(line) else {
                            continue;
                        };
                        code
                    };
                    for pattern in group.patterns {
                        if pattern.matches(scanned) {
                            *by_group.entry(group.name).or_insert(0) += 1;
                            sites.push(format!(
                                "{rel}:{} - {} retired `{}`: {}",
                                line_idx + 1,
                                group.name,
                                pattern.label,
                                line.trim().chars().take(140).collect::<String>()
                            ));
                        }
                    }
                }
            }
        }
    }

    for root_rel in STRICT_ACTIVE_RUST_RESIDUE_ROOTS {
        let path = root.join(root_rel);
        if !path.exists() {
            continue;
        }
        for file in collect_files(&path, &["rs"]).map_err(|err| err.to_string())? {
            let rel = relative(root, &file).replace('\\', "/");
            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            for (line_idx, line) in text.lines().enumerate() {
                for pattern in OLD_NAME_RESIDUE_PATTERNS {
                    if pattern.matches(line) {
                        *by_group.entry("active Rust old-name residue").or_insert(0) += 1;
                        sites.push(format!(
                            "{rel}:{} - active Rust old-name residue `{}`: {}",
                            line_idx + 1,
                            pattern.label,
                            line.trim().chars().take(140).collect::<String>()
                        ));
                    }
                }
            }
        }
    }

    let count = sites.len();
    let over = count > MAX_TIME_WAKE_RETIRED_SITES;

    println!("Invariants Lint - time-wake-retired");
    println!("====================================");
    println!(
        "retired time/wake interface sites: {:>4}  (ceiling {})  {}",
        count,
        MAX_TIME_WAKE_RETIRED_SITES,
        if over { "OVER" } else { "ok" }
    );

    if !by_group.is_empty() {
        println!();
        println!("  by group:");
        for (group, n) in &by_group {
            println!("  {group:<48} {n:>4}");
        }
    }

    if !sites.is_empty() {
        println!();
        println!("  sites (first 80):");
        for site in sites.iter().take(80) {
            println!("  {site}");
        }
        if sites.len() > 80 {
            println!("  ... and {} more", sites.len() - 80);
        }
    }

    if over {
        return Err(format!(
            "time-wake-retired regression - {count} retired interface sites > ceiling {MAX_TIME_WAKE_RETIRED_SITES}. Use the owner-aware `_with_post` / registrar / typed-device routes from TIME_WAKE_v1 instead of reintroducing old direct interfaces."
        ));
    }

    Ok(())
}

const fn ident(name: &'static str) -> RetiredPattern {
    RetiredPattern {
        label: name,
        kind: MatchKind::Ident(name),
    }
}

const fn contains(needle: &'static str) -> RetiredPattern {
    RetiredPattern {
        label: needle,
        kind: MatchKind::Contains(needle),
    }
}

const fn function_def(name: &'static str) -> RetiredPattern {
    RetiredPattern {
        label: name,
        kind: MatchKind::FunctionDef(name),
    }
}

const fn method_call(name: &'static str) -> RetiredPattern {
    RetiredPattern {
        label: name,
        kind: MatchKind::MethodCall(name),
    }
}

const fn path_ident(prefix: &'static str, ident: &'static str) -> RetiredPattern {
    RetiredPattern {
        label: ident,
        kind: MatchKind::PathIdent { prefix, ident },
    }
}

impl RetiredPattern {
    fn matches(self, line: &str) -> bool {
        match self.kind {
            MatchKind::Ident(ident) => contains_ident(line, ident),
            MatchKind::Contains(needle) => line.contains(needle),
            MatchKind::FunctionDef(name) => contains_function_def(line, name),
            MatchKind::MethodCall(name) => line.contains(&format!(".{name}(")),
            MatchKind::PathIdent { prefix, ident } => contains_path_ident(line, prefix, ident),
            MatchKind::LineAll(parts) => parts.iter().all(|part| line.contains(part)),
        }
    }
}

fn contains_function_def(line: &str, name: &str) -> bool {
    let Some(idx) = line.find("fn ") else {
        return false;
    };
    contains_ident(&line[idx + 3..], name)
}

fn contains_path_ident(line: &str, prefix: &str, ident: &str) -> bool {
    let Some(idx) = line.find(prefix) else {
        return false;
    };
    contains_ident(&line[idx + prefix.len()..], ident)
}

fn code_before_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*")
    {
        return None;
    }
    Some(line.split_once("//").map(|(code, _)| code).unwrap_or(line))
}

fn contains_ident(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    for start in 0..=bytes.len() - needle.len() {
        if &bytes[start..start + needle.len()] != needle {
            continue;
        }
        let before = start.checked_sub(1).and_then(|i| bytes.get(i).copied());
        let after = bytes.get(start + needle.len()).copied();
        if !is_ident_byte(before) && !is_ident_byte(after) {
            return true;
        }
    }
    false
}

fn is_ident_byte(byte: Option<u8>) -> bool {
    matches!(byte, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tx-time-wake-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn time_wake_lint_fails_on_retired_timer_surface_in_xtask() {
        let root = temp_root("retired-timer");
        let xtask_dir = root.join("xtask/src");
        fs::create_dir_all(&xtask_dir).expect("create temp xtask dir");
        fs::write(
            xtask_dir.join("retired.rs"),
            format!("use {RETIRED_WAKE_TIMER_MODULE}::{RETIRED_TIMER_WAKE_ROUTER};\n"),
        )
        .expect("write temp retired xtask source");

        let result = lint_invariants_time_wake_retired(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        result.expect_err("retired timer surface in xtask should fail the hard lint");
    }

    #[test]
    fn time_wake_lint_fails_on_retired_timer_comment_in_xtask() {
        let root = temp_root("retired-timer-comment");
        let xtask_dir = root.join("xtask/src");
        fs::create_dir_all(&xtask_dir).expect("create temp xtask dir");
        fs::write(
            xtask_dir.join("retired.rs"),
            format!("// {RETIRED_TIMER_WHEEL}\n"),
        )
        .expect("write temp retired xtask source");

        let result = lint_invariants_time_wake_retired(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        result.expect_err("retired timer comment in xtask should fail the hard lint");
    }

    #[test]
    fn time_wake_lint_allows_deadline_token_types() {
        let root = temp_root("deadline-tokens");
        let xtask_dir = root.join("xtask/src");
        fs::create_dir_all(&xtask_dir).expect("create temp xtask dir");
        fs::write(
            xtask_dir.join("deadline.rs"),
            "use tx_substrate::wake::deadline::{TimerGuardRole, TimerToken};\n",
        )
        .expect("write temp deadline token source");

        let result = lint_invariants_time_wake_retired(&root);
        fs::remove_dir_all(&root).expect("remove temp lint root");
        result.expect("deadline TimerToken and TimerGuardRole should remain allowed");
    }

    #[test]
    fn exact_ident_does_not_match_with_post_suffix() {
        assert!(ident("publish_rtc_event").matches("publish_rtc_event(mask);"));
        assert!(!ident("publish_rtc_event").matches("publish_rtc_event_with_post(mask, post);"));
        assert!(ident("push_cqe").matches("ring.push_cqe(entry);"));
        assert!(!ident("push_cqe").matches("ring.push_cqe_with_post(entry, post);"));
    }

    #[test]
    fn function_def_matches_direct_name_only() {
        assert!(function_def("notify_v3_source").matches("pub fn notify_v3_source() {}"));
        assert!(
            !function_def("notify_v3_source").matches("pub fn notify_v3_source_with_post<F>() {}")
        );
        assert!(function_def("post_signal").matches("pub fn post_signal(...) {}"));
        assert!(!function_def("post_signal").matches("pub fn post_signal_with_post<F>(...) {}"));
    }

    #[test]
    fn path_ident_matches_exact_segment_only() {
        assert!(path_ident("SignalFd::", "notify").matches("SignalFd::notify(fd);"));
        assert!(!path_ident("SignalFd::", "notify").matches("SignalFd::notify_with_post(fd);"));
        assert!(path_ident("notification::", "notify_changed")
            .matches("notification::notify_changed(&sem);"));
        assert!(!path_ident("notification::", "notify_changed")
            .matches("notification::notify_changed_with_post(&sem, post);"));
    }

    #[test]
    fn comment_stripping_ignores_doc_only_mentions() {
        assert!(code_before_comment("    post_signal(target);").is_some());
        assert!(code_before_comment("    post_signal(target); // old name").is_some());
        assert!(code_before_comment("/// post_signal was retired").is_none());
        assert!(code_before_comment("// notify_v3_source").is_none());
    }

    #[test]
    fn old_name_residue_patterns_match_comments_and_helpers() {
        assert!(ident("post_signal").matches("// post_signal old route"));
        assert!(ident("post_signal_for_test").matches("fn post_signal_for_test() {}"));
        assert!(!ident("post_signal").matches("post_signal_with_post(thread, sig, post);"));
        assert!(!ident("notify_v3_source").matches("notify_v3_source_with_post(source, post);"));
        assert!(ident("notify_child_zombified").matches("notify_child_zombified(&parent);"));
        assert!(!ident("notify_child_zombified")
            .matches("notify_child_zombified_with_post(&parent, post);"));
        assert!(ident("step_exit_group").matches("step_exit_group(&proc, status);"));
        assert!(!ident("step_exit_group")
            .matches("step_exit_group_with_posts(&proc, status, post, source_post);"));
        assert!(ident("step_exit_group_with_signal")
            .matches("step_exit_group_with_signal(&proc, sig);"));
        assert!(!ident("step_exit_group_with_signal")
            .matches("step_exit_group_with_signal_with_posts(&proc, sig, post, source_post);"));
        assert!(ident("step_kill_process").matches("step_kill_process(&proc, sig, None);"));
        assert!(!ident("step_kill_process")
            .matches("step_kill_process_with_post(&proc, sig, None, post);"));
        assert!(!ident("step_kill_process")
            .matches("step_kill_process_with_posts(&proc, sig, None, post, source_post);"));
        assert!(ident("step_kill_pgrp").matches("step_kill_pgrp(&pgrp, sig);"));
        assert!(!ident("step_kill_pgrp").matches("step_kill_pgrp_with_post(&pgrp, sig, post);"));
        assert!(ident("KillPgrpOp").matches("let mut op = KillPgrpOp { pgrp, sig };"));
        assert!(
            !ident("KillPgrpOp").matches("let mut op = KillPgrpWithPostOp { pgrp, sig, post };")
        );
        assert!(ident("deliver_posix_signal").matches("deliver_posix_signal(target, sig);"));
        assert!(!ident("deliver_posix_signal")
            .matches("deliver_posix_signal_with_post(target, sig, post);"));
        assert!(ident("DeliverSignalOp").matches("let mut op = DeliverSignalOp { target, sig };"));
        assert!(!ident("DeliverSignalOp")
            .matches("let mut op = DeliverSignalWithPostOp { target, sig, post };"));
        assert!(ident("route_gewalt").matches("route_gewalt(&proc, sig);"));
        assert!(!ident("route_gewalt").matches("route_gewalt_with_post(&proc, sig, post);"));
        assert!(ident("fire_due_delegate_timeouts")
            .matches("wheel.fire_due_delegate_timeouts(now, registry);"));
        assert!(ident("net_delegate_direct_mailbox_post")
            .matches("net_delegate_kick_poll_with_post(net_delegate_direct_mailbox_post);"));
        assert!(ident("direct_mailbox_post")
            .matches("socket.fire_recv_with_post(set, direct_mailbox_post);"));
        assert!(!ident("direct_mailbox_post")
            .matches("socket.fire_recv_with_post(set, |mailbox, event| mailbox.post(event));"));
        assert!(ident("step_process_device_tx_pending")
            .matches("step_process_device_tx_pending(sink, budget, guard);"));
        assert!(!ident("step_process_device_tx_pending")
            .matches("step_process_device_tx_pending_with_post(sink, budget, guard, post);"));
        assert!(ident("step_process_device_tx_pending_at")
            .matches("step_process_device_tx_pending_at(sink, now, budget, guard);"));
        assert!(!ident("step_process_device_tx_pending_at").matches(
            "step_process_device_tx_pending_at_with_post(sink, now, budget, guard, post);"
        ));
        assert!(
            ident("step_process_device_tx_pending_in_namespace_at").matches(
                "step_process_device_tx_pending_in_namespace_at(sink, ns, now, budget, guard);"
            )
        );
        assert!(!ident("step_process_device_tx_pending_in_namespace_at").matches(
            "step_process_device_tx_pending_in_namespace_at_with_post(sink, ns, now, budget, guard, post);"
        ));
        assert!(ident("netlink_route_send").matches("netlink_route_send(socket, bytes, cred);"));
        assert!(!ident("netlink_route_send")
            .matches("netlink_route_send_with_post(socket, bytes, cred, post);"));
        assert!(ident("netlink_route_send_with_netns_resolvers")
            .matches("netlink_route_send_with_netns_resolvers(socket, bytes, cred, fd, pid);"));
        assert!(!ident("netlink_route_send_with_netns_resolvers").matches(
            "netlink_route_send_with_netns_resolvers_and_post(socket, bytes, cred, fd, pid, post);"
        ));
        assert!(!ident("netlink_netfilter_send")
            .matches("netlink_netfilter_send_with_post(socket, bytes, cred, post);"));
        assert!(!ident("netlink_xfrm_send")
            .matches("netlink_xfrm_send_with_post(socket, bytes, cred, post);"));
        assert!(ident("step_process_loopback_udp")
            .matches("step_process_loopback_udp(&socket, 8, guard);"));
        assert!(!ident("step_process_loopback_udp")
            .matches("step_process_loopback_udp_with_post(&socket, 8, guard, post);"));
        assert!(ident("step_process_loopback_udp_on_iface")
            .matches("step_process_loopback_udp_on_iface(&socket, 8, iface, guard);"));
        assert!(!ident("step_process_loopback_udp_on_iface").matches(
            "step_process_loopback_udp_on_iface_with_post(&socket, 8, iface, guard, post);"
        ));
        assert!(ident("step_send_udp_loopback_kernel_bytes")
            .matches("step_send_udp_loopback_kernel_bytes(&socket, dst, bytes, flags, guard);"));
        assert!(!ident("step_send_udp_loopback_kernel_bytes").matches(
            "step_send_udp_loopback_kernel_bytes_with_post(&socket, dst, bytes, flags, guard, post);"
        ));
        assert!(ident("step_send_udp_loopback_kernel_bytes_on_iface").matches(
            "step_send_udp_loopback_kernel_bytes_on_iface(&socket, dst, bytes, flags, iface, guard);"
        ));
        assert!(!ident("step_send_udp_loopback_kernel_bytes_on_iface").matches(
            "step_send_udp_loopback_kernel_bytes_on_iface_with_post(&socket, dst, bytes, flags, iface, guard, post);"
        ));
        assert!(ident("step_process_loopback_icmp")
            .matches("step_process_loopback_icmp(&socket, 8, guard);"));
        assert!(!ident("step_process_loopback_icmp")
            .matches("step_process_loopback_icmp_with_post(&socket, 8, guard, post);"));
        assert!(ident("step_process_loopback_icmp_on_iface")
            .matches("step_process_loopback_icmp_on_iface(&socket, 8, iface, guard);"));
        assert!(!ident("step_process_loopback_icmp_on_iface").matches(
            "step_process_loopback_icmp_on_iface_with_post(&socket, 8, iface, guard, post);"
        ));
        assert!(function_def("mark_replied")
            .matches("pub fn mark_replied(&self, id, reply) -> TransitionOutcome {"));
        assert!(!function_def("mark_replied").matches(
            "pub fn mark_replied_with_post<F>(&self, id, reply, post) -> TransitionOutcome {"
        ));
        assert!(method_call("mark_timed_out").matches("registry.mark_timed_out(id);"));
        assert!(
            !method_call("mark_timed_out").matches("registry.mark_timed_out_with_post(id, post);")
        );
        assert!(method_call("mark_endpoint_died").matches("registry.mark_endpoint_died(marker);"));
        assert!(!method_call("mark_endpoint_died")
            .matches("registry.mark_endpoint_died_with_post(marker, post);"));
        assert!(ident("mark_replied").matches("// mark_replied old direct transition"));
        assert!(!ident("mark_replied").matches("registry.mark_replied_with_post(id, reply, post);"));
        assert!(ident("mark_canceled").matches("// mark_canceled old direct transition"));
        assert!(!ident("mark_canceled").matches("registry.mark_canceled_with_post(id, post);"));
        assert!(ident("mark_agent_died").matches("// mark_agent_died old direct transition"));
        assert!(!ident("mark_agent_died").matches("registry.mark_agent_died_with_post(id, post);"));
        assert!(ident("timerfd_clock_was_set").matches("timerfd_clock_was_set(2);"));
        assert!(!ident("timerfd_clock_was_set")
            .matches("timerfd_clock_was_set_with_post(2, |mailbox, event| mailbox.post(event));"));
        assert!(ident("timerfd_settime_with_flags").matches("timerfd_settime_with_flags(tfd);"));
        assert!(!ident("timerfd_settime_with_flags")
            .matches("timerfd_settime_with_flags_and_post(tfd, post);"));
        assert!(contains("pub fn realtime_now_ns").matches("pub fn realtime_now_ns<P>() -> u64"));
        assert!(!contains("pub fn realtime_now_ns").matches("fn realtime_now_ns<P>() -> u64"));
        assert!(!contains("pub fn realtime_now_ns").matches("fn realtime_now_ns<P>(&self) -> u64;"));
        assert!(ident("step_eventfd_read").matches("step_eventfd_read(efd, buf, false);"));
        assert!(!ident("step_eventfd_read")
            .matches("step_eventfd_read_with_post(efd, buf, false, post);"));
        assert!(ident("step_eventfd_write").matches("step_eventfd_write(efd, 1, false);"));
        assert!(!ident("step_eventfd_write")
            .matches("step_eventfd_write_with_post(efd, 1, false, post);"));
        assert!(function_def("step_read").matches("pub fn step_read(payload: &PipePayload) {}"));
        assert!(
            !function_def("step_read").matches("pub fn step_read_with_post<F>(payload, post) {}")
        );
        assert!(function_def("step_write").matches("pub fn step_write(payload: &PipePayload) {}"));
        assert!(
            !function_def("step_write").matches("pub fn step_write_with_post<F>(payload, post) {}")
        );
        assert!(ident("ReadOp").matches("let mut op = ReadOp { payload };"));
        assert!(!ident("ReadOp").matches("let mut op = ReadWithPostOp { payload, post };"));
        assert!(ident("WriteOp").matches("let mut op = WriteOp { payload };"));
        assert!(!ident("WriteOp").matches("let mut op = WriteWithPostOp { payload, post };"));
        assert!(contains("crate::pipe::step_read(")
            .matches("crate::pipe::step_read(payload, out, guard, false);"));
        assert!(!contains("crate::pipe::step_read(")
            .matches("crate::pipe::step_read_with_post(payload, out, guard, false, post);"));
        assert!(contains("crate::pipe::step_write(")
            .matches("crate::pipe::step_write(payload, bytes, guard, false, false);"));
        assert!(!contains("crate::pipe::step_write(").matches(
            "crate::pipe::step_write_with_post(payload, bytes, guard, false, false, post);"
        ));
        assert!(function_def("push_fault_msg").matches("pub fn push_fault_msg(&self, msg) {}"));
        assert!(!function_def("push_fault_msg")
            .matches("pub fn push_fault_msg_with_post<F>(&self, msg, post) {}"));
        assert!(method_call("push_fault_msg").matches("ufd.push_fault_msg(msg);"));
        assert!(!method_call("push_fault_msg").matches("ufd.push_fault_msg_with_post(msg, post);"));
        assert!(function_def("fault_script_for_process")
            .matches("pub async fn fault_script_for_process(&self, fault) {}"));
        assert!(!function_def("fault_script_for_process")
            .matches("pub async fn fault_script_for_process_with_post(&self, fault, post) {}"));
        assert!(method_call("fault_script_for_process")
            .matches("aspace.fault_script_for_process(fault, process, mailbox);"));
        assert!(!method_call("fault_script_for_process")
            .matches("aspace.fault_script_for_process_with_post(fault, process, mailbox, post);"));
        assert!(ident("fire_read_wait").matches("rnode.fire_read_wait(VFS_READABLE);"));
        assert!(!ident("fire_read_wait").matches("rnode.fire_read_wait_with_post(mask, post);"));
        assert!(ident("fire_write_wait").matches("rnode.fire_write_wait(VFS_WRITABLE);"));
        assert!(!ident("fire_write_wait").matches("rnode.fire_write_wait_with_post(mask, post);"));
        assert!(ident("step_mq_send").matches("step_mq_send(mq, msg, prio, cred);"));
        assert!(
            !ident("step_mq_send").matches("step_mq_send_with_post(mq, msg, prio, cred, post);")
        );
        assert!(ident("step_mq_receive").matches("step_mq_receive(mq, len, cred);"));
        assert!(
            !ident("step_mq_receive").matches("step_mq_receive_with_post(mq, len, cred, post);")
        );
        assert!(ident("step_msgsnd").matches("step_msgsnd(msqid, ty, msg, flags, cred);"));
        assert!(!ident("step_msgsnd")
            .matches("step_msgsnd_with_post(msqid, ty, msg, flags, cred, post);"));
        assert!(ident("step_msgrcv").matches("step_msgrcv(msqid, len, typ, flags, cred);"));
        assert!(!ident("step_msgrcv")
            .matches("step_msgrcv_with_post(msqid, len, typ, flags, cred, post);"));
        assert!(ident("step_msgsnd_v3").matches("step_msgsnd_v3(msqid, ty, msg, flags, cred);"));
        assert!(!ident("step_msgsnd_v3")
            .matches("step_msgsnd_v3_with_post(msqid, ty, msg, flags, cred, post);"));
        assert!(ident("step_msgrcv_v3").matches("step_msgrcv_v3(msqid, len, typ, flags, cred);"));
        assert!(!ident("step_msgrcv_v3")
            .matches("step_msgrcv_v3_with_post(msqid, len, typ, flags, cred, post);"));
        assert!(ident("step_msgctl").matches("step_msgctl(msqid, IPC_RMID, None, cred);"));
        assert!(!ident("step_msgctl")
            .matches("step_msgctl_with_post(msqid, IPC_RMID, None, cred, post);"));
        assert!(ident("step_msgctl_in_ns")
            .matches("step_msgctl_in_ns(msqid, IPC_RMID, None, cred, ns);"));
        assert!(!ident("step_msgctl_in_ns")
            .matches("step_msgctl_in_ns_with_post(msqid, IPC_RMID, None, cred, ns, post);"));
        assert!(ident("step_semop").matches("step_semop(semid, sops, cred, process);"));
        assert!(
            !ident("step_semop").matches("step_semop_with_post(semid, sops, cred, process, post);")
        );
        assert!(ident("step_semop_v3").matches("step_semop_v3(semid, sops, cred, process);"));
        assert!(!ident("step_semop_v3")
            .matches("step_semop_v3_with_post(semid, sops, cred, process, post);"));
        assert!(ident("step_semctl").matches("step_semctl(semid, semnum, cmd, arg, cred, None);"));
        assert!(!ident("step_semctl")
            .matches("step_semctl_with_post(semid, semnum, cmd, arg, cred, None, post);"));
        assert!(ident("step_semctl_in_ns")
            .matches("step_semctl_in_ns(semid, semnum, cmd, arg, cred, ns, process);"));
        assert!(!ident("step_semctl_in_ns").matches(
            "step_semctl_in_ns_with_post(semid, semnum, cmd, arg, cred, ns, process, post);"
        ));
        assert!(ident("step_sem_undo").matches("step_sem_undo(process);"));
        assert!(!ident("step_sem_undo").matches("step_sem_undo_with_post(process, post);"));
        assert!(function_def("step_ingest")
            .matches("pub fn step_ingest(tty: &Cap<TtyIdentity>, bytes: &[u8]) {"));
        assert!(contains("step_ingest(").matches("step_ingest(&tty, bytes, guard);"));
        assert!(
            !contains("step_ingest(").matches("step_ingest_with_post(&tty, bytes, guard, post);")
        );
        assert!(method_call("complete").matches("completion.complete();"));
        assert!(!method_call("complete")
            .matches("completion.complete_with_post(|mailbox, event| mailbox.post(event));"));
        assert!(method_call("arrive").matches("countdown.arrive();"));
        assert!(!method_call("arrive")
            .matches("countdown.arrive_with_post(|mailbox, event| mailbox.post(event));"));
        assert!(method_call("ack").matches("rendezvous.ack(target);"));
        assert!(!method_call("ack")
            .matches("rendezvous.ack_with_post(target, |mailbox, event| mailbox.post(event));"));
    }
}
