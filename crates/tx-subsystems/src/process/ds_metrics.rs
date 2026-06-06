//! Process data-structure method timing.
//!
//! Enabled only when both the global `tx_ds_metrics` gate and the
//! process-local `tx_ds_metrics_process` gate are open.

const DS_METRIC_DURATION_NS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(b"debug.ds.method.duration_ns"));

pub const PROCESS_DS_METHOD_NAMES: &[&[u8]] = &[
    b"debug.ds.process.pid_namespace.register_pid",
    b"debug.ds.process.pid_namespace.register_tid",
    b"debug.ds.process.pid_namespace.register_pgrp",
    b"debug.ds.process.pid_namespace.register_session",
    b"debug.ds.process.pid_namespace.unregister_pid_number",
    b"debug.ds.process.pid_namespace.unregister_tid_number",
    b"debug.ds.process.pid_namespace.resolve_pid_number",
    b"debug.ds.process.pid_namespace.resolve_pid_number_as",
    b"debug.ds.process.pid_namespace.with_namespace",
    b"debug.ds.process.children.attach",
    b"debug.ds.process.children.detach",
    b"debug.ds.process.children.len",
    b"debug.ds.process.children.is_empty",
    b"debug.ds.process.children.snapshot",
    b"debug.ds.process.children.drain",
    b"debug.ds.process.children.retain",
    b"debug.ds.process.group_members.attach",
    b"debug.ds.process.group_members.detach",
    b"debug.ds.process.group_members.len",
    b"debug.ds.process.group_members.is_empty",
    b"debug.ds.process.group_members.retain",
    b"debug.ds.process.group_members.snapshot_live",
    b"debug.ds.process.group_members.count_live",
    b"debug.ds.process.threads.attach",
    b"debug.ds.process.threads.detach",
    b"debug.ds.process.threads.count",
    b"debug.ds.process.threads.nth",
    b"debug.ds.process.threads.find_by_tid",
    b"debug.ds.process.threads.snapshot",
    b"debug.ds.process.threads.drain",
    b"debug.ds.process.threads.retain",
    b"debug.ds.process.session_members.attach",
    b"debug.ds.process.session_members.len",
    b"debug.ds.process.session_members.is_empty",
    b"debug.ds.process.session_members.snapshot_live",
];

#[inline(always)]
pub fn measure<R>(method_name: &'static [u8], f: impl FnOnce() -> R) -> R {
    let method = tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(method_name));
    let start = tx_observe::clock_now_ns();
    let result = f();
    let duration = tx_observe::clock_now_ns().saturating_sub(start);
    if let Some(emitter) = tx_observe::current() {
        emitter.ds_method_metric(method, DS_METRIC_DURATION_NS, duration);
    }
    result
}
