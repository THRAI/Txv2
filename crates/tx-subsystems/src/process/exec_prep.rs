//! Process-side helpers the exec script invokes between aspace swap
//! and userspace entry: close CLOEXEC fds, reset signal dispositions
//! to default, install the new brk base.
//!
//! Split out of `execution.rs` to keep file sizes within the
//! per-file authoring cap (1500 lines). These three helpers form a
//! coherent "post-aspace-swap, pre-user-entry" group called only from
//! the exec script.

use alloc::vec::Vec;

use crate::process::adapter::step_engine::{self, Cap};

use crate::process::structure::{ProcessIdentity, ProcessPayload};
use crate::vfs::OpenFile;

/// Close every fd marked `CLOEXEC` in `process.fd_table`, then clear
/// the `cloexec` set.
///
/// Per `txdoc:EXEC-12-2-CLOSE-CLOEXEC-FDS`. Called inside `exec_script`
/// after the process passes EXEC-PONR and before the new userspace
/// entry point runs. Infallible — closed slots release their
/// `Cap<OpenFile>` per zone EBR. If the removed descriptors hold the
/// final references to an open file, run its synchronous last-close
/// hook before dropping the caps. This is required for `SOCK_CLOEXEC`
/// exec-error channels: the peer must observe EOF when exec succeeds.
///
/// V1 ceiling: the cloexec set is internally a `u32` bitmap covering
/// fds 0..32. PR-FD-V scaffolding (in `process/fd_table.rs`) will
/// promote this to a `BitVec` once the per-process fd table grows
/// beyond 8 slots; the helper signature here stays the same. The
/// "8-slot ceiling" comment in the original `execution.rs` site
/// foreshadowed that growth; per the post-PR-FD-IV slide that ceiling
/// has been removed alongside the fd table's 8-slot ceiling; any `u32`
/// fd may be marked.
pub fn step_close_cloexec_fds(process: &Cap<ProcessIdentity>) {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let cloexec = process.fd_cloexec_snapshot();
    if cloexec.is_empty() {
        return;
    }
    let mut closed = Vec::<Cap<OpenFile>>::new();
    for fd in cloexec {
        if let Some(file) = process.set_fd(fd, None) {
            closed.push(file);
        }
    }
    // Clear the set wholesale: every previously-marked fd is now
    // closed; future fcntl(F_SETFD) calls start from a clean state.
    process.clear_fd_cloexec();

    let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
    let _ = super::execution::finalize_detached_open_files(&closed, &guard);
}

/// Reset every user-installed signal disposition on `process` to
/// `SigDisposition::Default`, preserving `Default` and `Ignore` slots.
///
/// Thin Phase-5 wrapper around
/// [`crate::signal::SigActionTable::step_reset_for_exec`] (Wave 2 P2)
/// that lets the exec script (`tx-scripts::process::exec`) reach the
/// per-process action table without touching the `pub(crate)` payload
/// field on [`ProcessIdentity`]. Per
/// `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS` and `SIGNAL_v1` §15.2:
/// exec resets handlers but does NOT clear pending signals or
/// SIG_IGN dispositions.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload).
pub fn step_reset_signal_dispositions_for_exec(process: &Cap<ProcessIdentity>) {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if let Some(payload) = process.payload.lock().as_ref() {
        payload.sig_actions().step_reset_for_exec();
    }
}

/// Install `new_brk_base` as both the brk base and the current brk
/// for `process`. Per `txdoc:EXEC-12-4-INSTALL-BRK` and the Wave 2
/// plan's Part 1 P3 sub-item.
///
/// The exec script (Part 5) computes `new_brk_base` from the image
/// plan: typically the highest LOAD segment's `vaddr + memsz`,
/// page-rounded up. Storing the same value into both fields seeds the
/// process at "no heap allocated yet" — `brk(2)` with a request above
/// `current_brk` then grows the heap on demand.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload to seed).
pub fn step_install_brk_for_exec(process: &Cap<ProcessIdentity>, new_brk_base: u64) {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if let Some(payload) = process.payload.lock().as_ref() {
        payload
            .brk_base
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
        payload
            .current_brk
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
    }
}

/// Store the new exec identity: command-line, executable DEntry, and
/// process short name (comm). Called inside `exec_script` after
/// Phase 6 (aspace swap) and before userspace entry.
///
/// Per `txdoc:EXEC-12-5-INSTALL-EXEC-IDENTITY`. Infallible — by
/// EXEC-PONR. No-op for zombies.
///
/// `cmdline` is the full argv as a flat NUL-separated byte slice.
/// `exe_dentry` is the resolved DEntry of the loaded binary.
/// `comm_bytes` is the basename of the executable (≤15 bytes).
pub fn step_store_exec_identity(
    process: &Cap<ProcessIdentity>,
    cmdline: &[u8],
    exe_dentry: Cap<crate::vfs::DEntry>,
    comm_bytes: &[u8],
) {
    // observe: payload cap (zombie guard)
    let Some(payload_cap) = process.payload.lock().clone() else {
        return;
    };
    let payload: &ProcessPayload = &payload_cap;
    // upgrade: (no witness needed — payload is Copy-accessible)
    // reserve: (no slot allocation — overwriting existing Option slots)
    // commit: write cmdline, exe_file, comm atomically
    *payload._cmdline.lock() = Some(cmdline.to_vec());
    *payload._exe_file.lock() = Some(exe_dentry);
    let mut buf = [0u8; 16];
    let len = (comm_bytes.len()).min(15);
    buf[..len].copy_from_slice(&comm_bytes[..len]);
    *payload._comm.lock() = buf;
    // publish: (no publication — fields are polled by procfs/readlink)
}
