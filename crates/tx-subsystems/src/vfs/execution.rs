//! VFS execution: backend trait and mutating step bodies.
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1` §execution: this module owns the `FsOps`
//! trait — the boundary that filesystem backends implement — plus the
//! mount-time output value `MountOutput` and the `OpenFile` step methods
//! that dispatch read/write through the RNode backing.

use alloc::sync::Arc;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::page_backed::FsPageBacking;
use crate::tty;

use super::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InodeMeta, OpenFile, OpenFileIoctl,
    OpenFileIoctlCaller, OpenFileIoctlResult, RNodeBacking, StructPayload,
};

/// Filesystem backend trait. The boundary tx-ext4, tmpfs, devfs, etc.
/// implement to provide namespace + page-backing operations.

// === FsOps — parallel trait emitting v3 step outcomes ================
//
// Wave-8 design + prototype slice. Per
// `docs/progress/decisions/2026-05-09-fsops-v3-design.md`, the trait-
// migration cascade is shaped differently from the free-fn cascades
// (waves 4/6/7) because trait surfaces — not call sites — choose how
// to map v4's `Advanced(t)` onto v3's `Continue { progress }` /
// `Yield { progress, … }` split. A default-method shim on `FsOps`
// (default `lookup_v3` calling `lookup` and converting) cannot make
// that decision — `Advanced(t)` is per-call-site ambiguous (Continue
// vs Done), and the default body has no caller context. So we grow
// `FsOps` as a parallel trait. Each FS impl block grows a sibling
// `impl FsOps for X` next to its existing `impl FsOps for X`. The
// final wholesale cascade (replacing FsOps with FsOps) lives in a
// later wave once all eight impls + walker callers are dual-routed.
//
// Per-method progress-type choice: every method in `FsOps` uses
// `step_v3::NoProgress`. The trait surface is one-shot identity-side
// queries / mutations (`lookup`, `mkdir`, `unlink`, …): the caller
// asks one question per call, and the trait's contract has no
// sub-operation accumulation (`readdir` returns one entry per call;
// the caller composes by re-calling with the new cursor — the cursor
// is a method input, not progress). Page-counting accumulators
// (`PageProgress`) live on the page-backing trait surface
// (`FsPageBacking`, wave 9 design), where ops like `flush_page`
// genuinely move pages. Cross-trait coupling: `FsOps::materialise_rnode`
// returns `Cap<RNode>` and the caller (`walker`) routes between
// FsOps and FsPageBacking via a single `MountPayload`; designing
// FsOps first leaves the FsPageBacking shape consistent and
// validates the approach with the smaller surface.
//
// Doc tag: `txdoc:STEP-V2-OUTCOME-ALGEBRA-1` (closed four-variant
// outcome). The `FsOps` declaration site is referenced by the
// design doc at `docs/progress/decisions/2026-05-09-fsops-v3-design.md`.

/// Parallel `FsOps` trait emitting v3 step outcomes.
///
/// Mirrors the 13-method shape of [`FsOps`] one-for-one, with every
/// `StepOutcome<T>` replaced by
/// `tx_substrate::step_v3::StepOutcome<T, NoProgress>`. Defaults match
/// `FsOps` exactly so projection-only / device-only backends inherit
/// `ENOSYS` without per-impl boilerplate.
///
/// Wave-8 introduces this trait and one prototype impl
/// (`LifecycleFs`); wave 9 fans out to the remaining seven impls
/// (`Tmpfs`, `Devfs`, `Ext4FsInstance`, `DevptsInstance`, `TestFs`,
/// `ExecTestFs`, `ExecveTestFs`). The eventual final cascade
/// replaces `FsOps` outright with `FsOps`.
pub trait FsOps: Send + Sync + 'static {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress>;

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress>;

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    >;

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    >;

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    >;

    /// Per-call one-entry readdir. Cursor is a method input, not
    /// progress — the caller composes multi-entry enumerations by
    /// re-calling with the returned cursor. The trait surface is
    /// `NoProgress`; if a later wave grows a multi-entry-per-call
    /// `readdir` it should live on a new method (e.g. `readdir_batch`)
    /// carrying `EntryProgress`.
    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: super::structure::DirCursor,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, super::structure::DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    >;

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress>;

    /// Read a symlink's target bytes. Default returns `ENOSYS` (parity
    /// with [`FsOps::read_link`]).
    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        alloc::boxed::Box<[u8]>,
        tx_substrate::step_v3::NoProgress,
    > {
        let _ = (fs_object_id, guard);
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    /// Backend hook for materialising an `RNode` for a non-directory,
    /// non-symlink inode. Default returns `ENOSYS` (parity with
    /// [`FsOps::materialise_rnode`]).
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        tx_substrate::zone::Cap<crate::vfs::structure::RNode>,
        tx_substrate::step_v3::NoProgress,
    > {
        let _ = (fs_object_id, meta, guard);
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    /// Update the inode's mode bits. Default returns `ENOSYS` (parity
    /// with [`FsOps::step_chmod`]).
    fn step_chmod(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        let _ = (fs_object_id, new_mode, cred, guard);
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    /// Update the inode's `(uid, gid)`. Default returns `ENOSYS`
    /// (parity with [`FsOps::step_chown`]).
    fn step_chown(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        let _ = (fs_object_id, new_uid, new_gid, cred, guard);
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }
}

/// Filesystem driver output produced at mount time and consumed by Mount
/// to build the mount payload. Per `TX_EXT4_PLAN_v1_2.md` §pub-types and
/// `bringup_fs_specs_v_1` §root-output.
///
/// Wave 9c grew the `fs_ops` / `fs_page_backing` sibling fields
/// alongside the v4 `fs_ops` / `fs_page_backing` so the new walker
/// entry points (`step_walk` / `step_open`) can route through the
/// v3 trait surface end-to-end. Backends populate both pairs from the
/// same `Arc<Self>`; the existing `fs_ops_arc` / `fs_page_backing_arc`
/// factory methods on `Tmpfs`, `Devfs`, and `Ext4FsInstance` produce
/// the v3-typed `Arc`s.
pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

// === OpenFile read/write step dispatch ================================
//
// Phase D interface slice: full fd tables, UserBuf copying, page-backed
// file I/O, and projection schemas are still later work. TTY and raw
// char-device struct payloads already route through their owning
// subsystems.

impl OpenFile {
    /// Dispatch a read against this file's RNode backing.
    pub fn step_read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.read {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_read(tty, out, guard),
                StructPayload::CharDevice(binding) => binding.ops.read(out, guard),
                StructPayload::Pipe {
                    payload,
                    side: crate::pipe::PipeSide::Reader,
                } => crate::pipe::step_read(payload, out, guard, self.flags.nonblocking),
                // Wrong-side read against a writer-end RNode. The
                // OpenFileFlags.read=false guard above handles the
                // common case (writer-end OpenFiles never set read);
                // this arm guards against a misconstructed RNode.
                StructPayload::Pipe {
                    side: crate::pipe::PipeSide::Writer,
                    ..
                } => StepOutcome::Err(Errno::EBADF),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }

    /// Reposition the per-fd offset.
    ///
    /// fd-ops Wave 4. Linux semantics:
    ///
    /// - `whence == SEEK_SET (0)`: new offset = `offset`.
    /// - `whence == SEEK_CUR (1)`: new offset = current + `offset`.
    /// - `whence == SEEK_END (2)`: new offset = file size + `offset`.
    ///   Only `RNodeBacking::PageBacked` carries a meaningful size;
    ///   other backings short-circuit before consulting size.
    ///
    /// Errors:
    /// - `EINVAL` for unknown `whence`, negative resulting offset,
    ///   or arithmetic overflow.
    /// - `ESPIPE` for non-seekable backings (`StructPayload::Tty`,
    ///   `CharDevice`, `Pipe`).
    /// - `EISDIR` for directory backings.
    /// - `ENOSYS` for symlink / projected backings (Wave 4 doesn't
    ///   expose those through any open path; defence in depth).
    ///
    /// `lseek` is a non-async, non-blocking step — no `Blocked` /
    /// `AdvancedThenBlocked` outcomes are reachable. The `Guard` is
    /// accepted for symmetry with the other `OpenFile::step_*`
    /// methods even though the body never crosses an EBR boundary.
    pub fn step_lseek(&self, offset: i64, whence: u32, _guard: &Guard<'_>) -> StepOutcome<u64> {
        // Backing-driven dispatch: short-circuit non-seekable
        // backings before any arithmetic. Pipes / TTY / chardev are
        // ESPIPE regardless of whence (Linux's `lseek(2)` man page:
        // "lseek() may, but need not, return -1 with errno set to
        // ESPIPE when offset is 0; portable code must treat any
        // result other than the requested offset as an error").
        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(_)
                | StructPayload::CharDevice(_)
                | StructPayload::Pipe { .. } => return StepOutcome::Err(Errno::ESPIPE),
            },
            RNodeBacking::Directory => return StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::Symlink { .. } | RNodeBacking::Projected => {
                return StepOutcome::Err(Errno::ENOSYS)
            }
            RNodeBacking::PageBacked { .. } => {}
        }

        // PageBacked branch: compute the new offset based on whence.
        let new_offset: i64 = match whence {
            // SEEK_SET
            0 => offset,
            // SEEK_CUR
            1 => match (self.offset() as i64).checked_add(offset) {
                Some(o) => o,
                None => return StepOutcome::Err(Errno::EINVAL),
            },
            // SEEK_END
            2 => {
                let size = match self.rnode.backing() {
                    RNodeBacking::PageBacked { pc } => pc.size_bytes() as i64,
                    // The outer match above already short-circuited
                    // every non-PageBacked backing; this arm is
                    // unreachable. Keep it as a defence-in-depth
                    // EINVAL rather than a panic.
                    _ => return StepOutcome::Err(Errno::EINVAL),
                };
                match size.checked_add(offset) {
                    Some(o) => o,
                    None => return StepOutcome::Err(Errno::EINVAL),
                }
            }
            _ => return StepOutcome::Err(Errno::EINVAL),
        };

        if new_offset < 0 {
            return StepOutcome::Err(Errno::EINVAL);
        }

        let new_offset_u64 = new_offset as u64;
        self.set_offset(new_offset_u64);
        StepOutcome::Done(new_offset_u64)
    }

    /// Dispatch a write against this file's RNode backing.
    pub fn step_write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize> {
        if !self.flags.write {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_write(tty, bytes, guard),
                StructPayload::CharDevice(binding) => binding.ops.write(bytes, guard),
                StructPayload::Pipe {
                    payload,
                    side: crate::pipe::PipeSide::Writer,
                } => crate::pipe::step_write(payload, bytes, guard, self.flags.nonblocking),
                // Wrong-side write against a reader-end RNode.
                StructPayload::Pipe {
                    side: crate::pipe::PipeSide::Reader,
                    ..
                } => StepOutcome::Err(Errno::EBADF),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }

    /// Dispatch a typed ioctl against this file's RNode backing.
    ///
    /// This is the VFS-side bridge between future syscall request-number
    /// decoding and the already-typed subsystem helpers. The day-1 slice
    /// only wires TTY-backed files; other backings keep the existing
    /// "not implemented at this seam" behavior.
    pub fn step_ioctl(
        &self,
        caller: OpenFileIoctlCaller<'_>,
        request: OpenFileIoctl<'_>,
        guard: &Guard<'_>,
    ) -> StepOutcome<OpenFileIoctlResult> {
        match self.rnode.backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => step_tty_ioctl(tty, caller, request, guard),
                StructPayload::CharDevice(_) => StepOutcome::Err(Errno::ENOSYS),
                // Pipe was added on main; ioctl on a pipe returns
                // ENOTTY (matches Linux behaviour).
                StructPayload::Pipe { .. } => StepOutcome::Err(Errno::ENOTTY),
            },
            RNodeBacking::Directory => StepOutcome::Err(Errno::EISDIR),
            RNodeBacking::PageBacked { .. }
            | RNodeBacking::Symlink { .. }
            | RNodeBacking::Projected => StepOutcome::Err(Errno::ENOSYS),
        }
    }
}

fn step_tty_ioctl(
    tty_id: &tx_substrate::zone::Cap<crate::tty::structure::TtyIdentity>,
    caller: OpenFileIoctlCaller<'_>,
    request: OpenFileIoctl<'_>,
    guard: &Guard<'_>,
) -> StepOutcome<OpenFileIoctlResult> {
    match request {
        OpenFileIoctl::Tcgets => match tty::execution::step_ioctl_tcgets(tty_id, guard) {
            StepOutcome::Done(termios) => StepOutcome::Done(OpenFileIoctlResult::Termios(termios)),
            StepOutcome::Advanced(termios) => {
                StepOutcome::Advanced(OpenFileIoctlResult::Termios(termios))
            }
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked(termios, token) => {
                StepOutcome::AdvancedThenBlocked(OpenFileIoctlResult::Termios(termios), token)
            }
            StepOutcome::Err(err) => StepOutcome::Err(err),
        },
        OpenFileIoctl::Tcsets { termios } => {
            match tty::execution::step_ioctl_tcsets(tty_id, termios, guard) {
                StepOutcome::Done(side_effect) => {
                    StepOutcome::Done(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Advanced(side_effect) => {
                    StepOutcome::Advanced(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::AdvancedThenBlocked(side_effect, token) => {
                    StepOutcome::AdvancedThenBlocked(
                        OpenFileIoctlResult::SideEffect(side_effect),
                        token,
                    )
                }
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        OpenFileIoctl::Tiocgpgrp => match tty::execution::step_ioctl_tiocgpgrp(tty_id, guard) {
            StepOutcome::Done(pgid) => StepOutcome::Done(OpenFileIoctlResult::Pgrp(pgid)),
            StepOutcome::Advanced(pgid) => StepOutcome::Advanced(OpenFileIoctlResult::Pgrp(pgid)),
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked(pgid, token) => {
                StepOutcome::AdvancedThenBlocked(OpenFileIoctlResult::Pgrp(pgid), token)
            }
            StepOutcome::Err(err) => StepOutcome::Err(err),
        },
        OpenFileIoctl::Tiocspgrp { new_pgrp } => {
            match tty::execution::step_ioctl_tiocspgrp_for_process(
                tty_id,
                caller.process(),
                new_pgrp,
                guard,
            ) {
                StepOutcome::Done(side_effect) => {
                    StepOutcome::Done(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Advanced(side_effect) => {
                    StepOutcome::Advanced(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::AdvancedThenBlocked(side_effect, token) => {
                    StepOutcome::AdvancedThenBlocked(
                        OpenFileIoctlResult::SideEffect(side_effect),
                        token,
                    )
                }
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        OpenFileIoctl::Tiocgwinsz => match tty::execution::step_ioctl_tiocgwinsz(tty_id, guard) {
            StepOutcome::Done(winsize) => StepOutcome::Done(OpenFileIoctlResult::Winsize(winsize)),
            StepOutcome::Advanced(winsize) => {
                StepOutcome::Advanced(OpenFileIoctlResult::Winsize(winsize))
            }
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::AdvancedThenBlocked(winsize, token) => {
                StepOutcome::AdvancedThenBlocked(OpenFileIoctlResult::Winsize(winsize), token)
            }
            StepOutcome::Err(err) => StepOutcome::Err(err),
        },
        OpenFileIoctl::Tiocswinsz { winsize } => {
            match tty::execution::step_ioctl_tiocswinsz(tty_id, winsize, guard) {
                StepOutcome::Done(side_effect) => {
                    StepOutcome::Done(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Advanced(side_effect) => {
                    StepOutcome::Advanced(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::AdvancedThenBlocked(side_effect, token) => {
                    StepOutcome::AdvancedThenBlocked(
                        OpenFileIoctlResult::SideEffect(side_effect),
                        token,
                    )
                }
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        OpenFileIoctl::Tiocsctty => {
            match tty::execution::step_ioctl_tiocsctty_for_process(tty_id, caller.process(), guard)
            {
                StepOutcome::Done(side_effect) => {
                    StepOutcome::Done(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Advanced(side_effect) => {
                    StepOutcome::Advanced(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::AdvancedThenBlocked(side_effect, token) => {
                    StepOutcome::AdvancedThenBlocked(
                        OpenFileIoctlResult::SideEffect(side_effect),
                        token,
                    )
                }
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        OpenFileIoctl::Tiocnotty => {
            match tty::execution::step_ioctl_tiocnotty_for_process(tty_id, caller.process(), guard)
            {
                StepOutcome::Done(side_effect) => {
                    StepOutcome::Done(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Advanced(side_effect) => {
                    StepOutcome::Advanced(OpenFileIoctlResult::SideEffect(side_effect))
                }
                StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
                StepOutcome::AdvancedThenBlocked(side_effect, token) => {
                    StepOutcome::AdvancedThenBlocked(
                        OpenFileIoctlResult::SideEffect(side_effect),
                        token,
                    )
                }
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
    }
}
