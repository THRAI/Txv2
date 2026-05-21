//! Namespace-scoped step operations for IPC.
//!
//! `step_clone_newipc` — called from `clone(2)` with `CLONE_NEWIPC`.
//! `step_set_limits` — per-namespace tunable limit adjustment.
//!
//! Day-1 stub: single-namespace, limits inherited at init-bootstrap.
//! Real impl lands with `CLONE_NEWIPC` (clone_newipc) and
//! `/proc/sys/kernel/sem` write path (step_set_limits).

// TODO(txdoc:IPC-V1-NAMESPACE-1): implement CLONE_NEWIPC —
// create fresh IpcNamespace, copy parent limits, publish new NsProxy.
