//! VFS execution: backend trait and mutating step bodies.
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1` §execution: this module owns the `FsOps`
//! trait — the boundary that filesystem backends implement — plus the
//! mount-time output value `MountOutput` and the `OpenFile` step methods
//! that dispatch read/write through the RNode backing.

use alloc::sync::Arc;

use crate::execution::Guard;
use crate::page_backed::FsPageBacking;
use crate::tty;
use crate::vfs::adapter::step_engine::{
    ByteProgress, Cap, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};

use super::structure::{
    Credential, DirEntry, FsObjectId, InodeMeta, OpenFile, OpenFileBacking, OpenFileIoctl,
    OpenFileIoctlCaller, OpenFileIoctlResult, RNodeBacking, StructPayload,
};

// === FsOps — emits step_v3 outcomes ==================================
//
// Per-method progress-type choice: every method in `FsOps` uses
// `step_v3::NoProgress`. The trait surface is one-shot identity-side
// queries / mutations (`lookup`, `mkdir`, `unlink`, …): the caller
// asks one question per call, and the trait's contract has no
// sub-operation accumulation (`readdir` returns one entry per call;
// the caller composes by re-calling with the new cursor — the cursor
// is a method input, not progress). Page-counting accumulators
// (`PageProgress`) live on the page-backing trait surface
// (`FsPageBacking`), where ops like `flush_page` genuinely move pages.
// Cross-trait coupling: `FsOps::materialise_rnode` returns
// `Cap<RNode>` and the caller (`walker`) routes between `FsOps` and
// `FsPageBacking` via a single `MountPayload`.
//
// Doc tag: `txdoc:STEP-V2-OUTCOME-ALGEBRA-1` (closed four-variant
// outcome).

/// `FsOps` — canonical v3 filesystem operation vtable.
///
/// 13 methods returning
/// `StepOutcome<T, NoProgress>`. Defaults give
/// projection-only / device-only backends `ENOSYS` without per-impl
/// boilerplate.
///
/// `FsOps` is **not** the "v4 trait." It is the live VFS operation
/// boundary dispatched as `Arc<dyn FsOps>` from the shim layer
/// (`fs_basic.rs`, `fs_mut.rs`, `fs_path.rs`). Once its methods return
/// `step_v3::StepOutcome`, it is a v3 trait in substance — the name
/// is older than the v3 vocabulary, that is all. See
/// `docs/progress/decisions/2026-05-11-pr-1-6-keep-fsops.md` for the
/// rationale (v3 requires StepOutcome shape unification, not
/// trait-identity unification; deleting `FsOps` would force a
/// filesystem-dispatch redesign out of scope for v3).
///
/// Companion trait: [`crate::page_backed::FsPageBacking`] is the
/// page-cache / pager-facing backing surface. Page-cache-backed
/// filesystems implement both; the traits do not subsume each other.
pub trait FsOps: Send + Sync + 'static {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress>;

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress>;

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress>;

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
    ) -> StepOutcome<Option<(DirEntry, super::structure::DirCursor)>, NoProgress>;

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    /// Read a symlink's target bytes. Default returns `ENOSYS` (parity
    /// with [`FsOps::read_link`]).
    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        let _ = (fs_object_id, guard);
        StepOutcome::err(Errno::ENOSYS)
    }

    /// Backend hook for materialising an `RNode` for a non-directory,
    /// non-symlink inode. Default returns `ENOSYS` (parity with
    /// [`FsOps::materialise_rnode`]).
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<Cap<crate::vfs::structure::RNode>, NoProgress> {
        let _ = (fs_object_id, meta, guard);
        StepOutcome::err(Errno::ENOSYS)
    }

    /// Update the inode's mode bits. Default returns `ENOSYS` (parity
    /// with [`FsOps::step_chmod`]).
    fn step_chmod(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _ = (fs_object_id, new_mode, cred, guard);
        StepOutcome::err(Errno::ENOSYS)
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
    ) -> StepOutcome<(), NoProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        let _ = (fs_object_id, new_uid, new_gid, cred, guard);
        StepOutcome::err(Errno::ENOSYS)
    }
}

/// Filesystem driver output produced at mount time and consumed by Mount
/// to build the mount payload. Per `TX_EXT4_PLAN_v1_2.md` §pub-types and
/// `bringup_fs_specs_v_1` §root-output.
///
/// Backends populate `fs_ops` / `fs_page_backing` via the
/// `fs_ops_arc` / `fs_page_backing_arc` factory methods on `Tmpfs`,
/// `Devfs`, and `Ext4FsInstance`; the walker entry points
/// (`step_walk` / `step_open`) route through these trait objects
/// end-to-end.
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
    pub fn step_read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        // observe: validate file is readable
        // ① observe — flag check + backing dispatch
        // ② upgrade — (N/A: delegated to backing trait impl)
        // ③ reserve — (N/A: delegated to backing trait impl)
        // ④ commit — (N/A: delegated to backing trait impl)
        // ⑤ publish — (N/A: read doesn't fire signals)
        if !self.flags.read {
            return StepOutcome::Err(Errno::EINVAL);
        }

        // ② upgrade — (N/A: VFS dispatch doesn't upgrade witness)
        // ③ reserve — (N/A)
        // PR-10 phase 0: userfaultfd fds have no VFS-shaped read path.
        // The agent-side read syscall lands in P-10.5 with its own
        // dispatch (it dequeues a fault message, not bytes from a
        // file). Surface EINVAL until then.
        if matches!(self.backing(), OpenFileBacking::Ufd { .. }) {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode().backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_read(tty, out, guard),
                StructPayload::CharDevice(binding) => binding.ops.read(out, guard),
                StructPayload::BlockDevice(_) => StepOutcome::Err(Errno::ENOSYS),
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
            // PR-11 follow-up (W-KK, closing the ENOSYS gap W-JJ flagged
            // in the PR-11 phase-6 AIO canary): route PageBacked reads
            // through the kernel-buffer page-backed read path. PC-side
            // page materialisation handles EOF / blocking / errors;
            // `of.offset()` is advanced inside the helper.
            RNodeBacking::PageBacked { pc } => {
                crate::page_backed::step_read_to_kernel(pc, self, out, guard)
            }
            RNodeBacking::Symlink { .. } | RNodeBacking::Projected => {
                StepOutcome::Err(Errno::ENOSYS)
            }
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
    ///
    /// Stages: observe/upgrade/reserve/commit/publish — all N/A.
    /// Pure arithmetic on inode metadata; no guard acquisition needed.
    pub fn step_lseek(
        // ① observe — backing-based seekability check
        &self,
        offset: i64,
        whence: u32,
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        // ① observe — backing kind check + offset computation
        // ② upgrade — (N/A: pure computation, no EBR guard used)
        // ③ reserve — (N/A: no resource reservation)
        // ④ commit — self.set_offset(new_offset_u64)
        // ⑤ publish — (N/A: lseek doesn't fire signals)
        // PR-10 phase 0: userfaultfd fds have no offset semantic.
        // Linux returns ESPIPE on `lseek(uffd_fd, ...)`; match that.
        if matches!(self.backing(), OpenFileBacking::Ufd { .. }) {
            return StepOutcome::Err(Errno::ESPIPE);
        }
        // Backing-driven dispatch: short-circuit non-seekable
        // backings before any arithmetic. Pipes / TTY / chardev are
        // ESPIPE regardless of whence (Linux's `lseek(2)` man page:
        // "lseek() may, but need not, return -1 with errno set to
        // ESPIPE when offset is 0; portable code must treat any
        // result other than the requested offset as an error").
        match self.rnode().backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(_)
                | StructPayload::CharDevice(_)
                | StructPayload::BlockDevice(_)
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
                let size = match self.rnode().backing() {
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
    pub fn step_write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        // ① observe — flag check + backing dispatch
        // ② upgrade — (N/A: delegated to backing trait impl)
        // ③ reserve — (N/A: delegated to backing trait impl)
        // ④ commit — (N/A: delegated to backing trait impl)
        // ⑤ publish — (N/A: write doesn't fire signals directly)
        // observe: validate file is writable
        if !self.flags.write {
            return StepOutcome::Err(Errno::EINVAL);
        }

        // PR-10 phase 0: userfaultfd fds have no VFS-shaped write path.
        // The agent-side `UFFDIO_*` ioctls (phase P-10.5) deliver the
        // reply path, not write(2). Surface EINVAL until then.
        if matches!(self.backing(), OpenFileBacking::Ufd { .. }) {
            return StepOutcome::Err(Errno::EINVAL);
        }

        match self.rnode().backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => tty::execution::step_write(tty, bytes, guard),
                StructPayload::CharDevice(binding) => binding.ops.write(bytes, guard),
                StructPayload::BlockDevice(_) => StepOutcome::Err(Errno::ENOSYS),
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
            // Symmetric to the PageBacked step_read arm above — route
            // through the kernel-buffer page-backed write path. The
            // helper handles capacity checks, page materialisation,
            // `of.offset()` advance, and `PC.size` growth.
            RNodeBacking::PageBacked { pc } => {
                // O_APPEND: seek to current EOF before each write. POSIX
                // requires this seek-and-write to be atomic; our model
                // approximates it by snapping the offset just before the
                // write helper consumes it.
                if self.flags.append {
                    self.set_offset(pc.size_bytes());
                }
                crate::page_backed::step_write_from_kernel(pc, self, bytes, guard)
            }
            RNodeBacking::Symlink { .. } | RNodeBacking::Projected => {
                StepOutcome::Err(Errno::ENOSYS)
            }
        }
    }

    /// Dispatch a typed ioctl against this file's RNode backing.
    ///
    /// This is the VFS-side bridge between future syscall request-number
    /// decoding and the already-typed subsystem helpers. The day-1 slice
    /// only wires TTY-backed files; other backings keep the existing
    /// "not implemented at this seam" behavior.
    pub fn step_ioctl(
        // ① observe — backing-based dispatch
        &self,
        caller: OpenFileIoctlCaller<'_>,
        request: OpenFileIoctl<'_>,
        guard: &Guard<'_>,
    ) -> StepOutcome<OpenFileIoctlResult, NoProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
        // ① observe — backing kind check + ioctl dispatch
        // ② upgrade — (N/A: delegated to tty::execution)
        // ③ reserve — (N/A: delegated to tty::execution)
        // ④ commit — (N/A: delegated to tty::execution)
        // ⑤ publish — (N/A: signal side effects handled by tty::step_ioctl_*)
        // PR-10 phase 0: the VFS ioctl surface (`OpenFileIoctl`) is
        // TTY-shaped; userfaultfd ioctls have their own request
        // catalog landing in P-10.2+. Return ENOTTY for ufd fds via
        // this dispatcher.
        if matches!(self.backing(), OpenFileBacking::Ufd { .. }) {
            return StepOutcome::Err(Errno::ENOTTY);
        }
        match self.rnode().backing() {
            RNodeBacking::StructBacked { payload } => match payload {
                StructPayload::Tty(tty) => step_tty_ioctl(tty, caller, request, guard),
                StructPayload::CharDevice(_) => StepOutcome::Err(Errno::ENOSYS),
                StructPayload::BlockDevice(_) => StepOutcome::Err(Errno::ENOSYS),
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
    tty_id: &Cap<crate::tty::structure::TtyIdentity>,
    caller: OpenFileIoctlCaller<'_>,
    request: OpenFileIoctl<'_>,
    guard: &Guard<'_>,
) -> StepOutcome<OpenFileIoctlResult, NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // v3 NoProgress outcomes are one-shot Done/Err (Yield/Continue
    // unreachable for tty ioctl bodies today). Wrap each call's `Done`
    // into the typed `OpenFileIoctlResult` constructor.
    fn wrap_result<T>(
        v3: StepOutcome<T, NoProgress>,
        wrap: impl FnOnce(T) -> OpenFileIoctlResult,
    ) -> StepOutcome<OpenFileIoctlResult, NoProgress> {
        match v3 {
            StepOutcome::Done(value) => StepOutcome::Done(wrap(value)),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            _ => StepOutcome::Err(Errno::EIO),
        }
    }

    match request {
        OpenFileIoctl::Tcgets => wrap_result(
            tty::execution::step_ioctl_tcgets(tty_id, guard),
            OpenFileIoctlResult::Termios,
        ),
        OpenFileIoctl::Tcsets { termios } => wrap_result(
            tty::execution::step_ioctl_tcsets(tty_id, termios, guard),
            OpenFileIoctlResult::SideEffect,
        ),
        OpenFileIoctl::Tiocgpgrp => wrap_result(
            tty::execution::step_ioctl_tiocgpgrp(tty_id, guard),
            OpenFileIoctlResult::Pgrp,
        ),
        OpenFileIoctl::Tiocspgrp { new_pgrp } => wrap_result(
            tty::execution::step_ioctl_tiocspgrp_for_process(
                tty_id,
                caller.process(),
                new_pgrp,
                guard,
            ),
            OpenFileIoctlResult::SideEffect,
        ),
        OpenFileIoctl::Tiocgwinsz => wrap_result(
            tty::execution::step_ioctl_tiocgwinsz(tty_id, guard),
            OpenFileIoctlResult::Winsize,
        ),
        OpenFileIoctl::Tiocswinsz { winsize } => wrap_result(
            tty::execution::step_ioctl_tiocswinsz(tty_id, winsize, guard),
            OpenFileIoctlResult::SideEffect,
        ),
        OpenFileIoctl::Tiocsctty => wrap_result(
            tty::execution::step_ioctl_tiocsctty_for_process(tty_id, caller.process(), guard),
            OpenFileIoctlResult::SideEffect,
        ),
        OpenFileIoctl::Tiocnotty => wrap_result(
            tty::execution::step_ioctl_tiocnotty_for_process(tty_id, caller.process(), guard),
            OpenFileIoctlResult::SideEffect,
        ),
    }
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 3)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
//
// Unlike most PR-2 wraps in sibling subsystems (which target free `step_*`
// fns), the VFS execution surface exposes its four `step_*` operations as
// inherent methods on `OpenFile` (`step_read`, `step_lseek`, `step_write`,
// `step_ioctl`). The wraps therefore hold a `&'a Cap<OpenFile>` and delegate
// from `step()` through `Cap::deref()` to the method body — semantics are
// unchanged. The `Cap<OpenFile>` is borrowed (not cloned) so the `Op` shape
// matches the other wave-3 byte-IO wraps (`pipe::ReadOp`, `tty::execution::
// step_read::ReadOp`).

/// `StepOp` wrap of [`OpenFile::step_read`].
pub struct OpenFileReadOp<'a> {
    pub file: &'a Cap<super::structure::OpenFile>,
    pub out: &'a mut [u8],
    pub guard: &'a Guard<'a>,
    /// Internal write cursor: each `step()` call fills bytes starting
    /// at `out[cursor..]` and advances `cursor` by the amount returned
    /// in the outcome. This allows `drive()` to call `step()` multiple
    /// times without the caller needing to update `out` between
    /// iterations (DRIVE-2).
    cursor: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileReadOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let result = self.file.step_read(&mut self.out[self.cursor..], self.guard);
        // Advance cursor by the bytes read in this step. The
        // `StepProgress` accumulator (ByteProgress) carries the same
        // value, so `drive()`'s `accumulated` stays in sync with the
        // actual fill position.
        match &result {
            StepOutcome::Done(n) => {
                self.cursor += *n as usize;
            }
            StepOutcome::Continue { progress } => {
                self.cursor += progress.bytes();
            }
            StepOutcome::Yield { progress, .. } => {
                self.cursor += progress.bytes();
            }
            StepOutcome::Err(_) => {}
        }
        result
    }
}

/// `StepOp` wrap of [`OpenFile::step_lseek`]. `offset` / `whence` are
/// scalar; stored by value.
pub struct OpenFileLseekOp<'a> {
    pub file: &'a Cap<super::structure::OpenFile>,
    pub offset: i64,
    pub whence: u32,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileLseekOp<'a> {
    type Output = u64;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        self.file.step_lseek(self.offset, self.whence, self.guard)
    }
}

/// `StepOp` wrap of [`OpenFile::step_write`].
pub struct OpenFileWriteOp<'a> {
    pub file: &'a Cap<super::structure::OpenFile>,
    pub bytes: &'a [u8],
    pub guard: &'a Guard<'a>,
    /// Internal write cursor: each `step()` call consumes bytes starting
    /// at `bytes[cursor..]`. Mirrors `OpenFileReadOp::cursor`.
    cursor: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileWriteOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let result = self.file.step_write(&self.bytes[self.cursor..], self.guard);
        match &result {
            StepOutcome::Done(n) => {
                self.cursor += *n as usize;
            }
            StepOutcome::Continue { progress } => {
                self.cursor += progress.bytes();
            }
            StepOutcome::Yield { progress, .. } => {
                self.cursor += progress.bytes();
            }
            StepOutcome::Err(_) => {}
        }
        result
    }
}

/// `StepOp` wrap of [`OpenFile::step_ioctl`]. The caller / request enums
/// borrow `'a`, so the wrap inherits the same lifetime.
pub struct OpenFileIoctlOp<'a> {
    pub file: &'a Cap<super::structure::OpenFile>,
    pub caller: OpenFileIoctlCaller<'a>,
    pub request: OpenFileIoctl<'a>,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for OpenFileIoctlOp<'a> {
    type Output = OpenFileIoctlResult;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        self.file.step_ioctl(self.caller, self.request, self.guard)
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave-3 StepOp wrap tests. Each test constructs a minimal
    //! `OpenFile` (TTY-, char-device-, or directory-backed) and confirms
    //! the wrap delegates to the corresponding `OpenFile::step_*` method.
    //! Coverage of the dispatch table itself lives in
    //! `crate::vfs::tests`.
    use super::*;
    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
    use crate::vfs::adapter::step_engine::{
        guard, reserve_for, sign_for, Cap, PayloadCap, ProcessIdentity, ScriptCtx, StepOp,
        StepOutcome as V3,
    };
    use crate::vfs::structure::{
        FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, OpenFileIoctl,
        OpenFileIoctlCaller, OpenFileIoctlResult, RNode, RNodeBacking, StructPayload,
    };
    use crate::zones;

    struct EchoOps;

    impl CharDeviceOps for EchoOps {
        fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            if out.is_empty() {
                return V3::Done(0);
            }
            out[0] = b'E';
            V3::Done(1)
        }

        fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
            V3::Done(bytes.len())
        }
    }

    static ECHO_OPS: EchoOps = EchoOps;
    static ECHO_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(241, 0),
        name: "echo-wraptest",
        ops: &ECHO_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        crate::tty::structure::registry::reset_for_tests();
        guard
    }

    fn make_char_open_file(read: bool, write: bool) -> Cap<OpenFile> {
        let rnode = RNode::new_cap(
            FsObjectId::new(1001),
            InodeMeta::new(InodeKind::CharDevice, 0o020600),
            RNodeBacking::StructBacked {
                payload: StructPayload::CharDevice(&ECHO_BINDING),
            },
        )
        .expect("char rnode");
        OpenFile::new_cap(
            rnode,
            OpenFileFlags {
                read,
                write,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("open file")
    }

    fn make_tty(index: u32, name: &str) -> Cap<TtyIdentity> {
        let id_res = reserve_for::<TtyIdentity>().expect("tty identity reservation");
        let payload_res = reserve_for::<TtyPayload>().expect("tty payload reservation");
        let payload = PayloadCap::from_cap(sign_for(
            payload_res,
            TtyPayload::new_hardware(&ECHO_BINDING),
        ));
        let identity = sign_for(
            id_res,
            TtyIdentity::new(TtyKind::SerialHardware, index, name),
        );
        identity.install_payload(payload);
        identity
    }

    fn make_tty_open_file(read: bool, write: bool, index: u32, name: &str) -> Cap<OpenFile> {
        let tty = make_tty(index, name);
        let rnode = RNode::new_cap(
            FsObjectId::new(2000 + u64::from(index)),
            InodeMeta::new(InodeKind::CharDevice, 0o020600),
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            },
        )
        .expect("tty rnode");
        OpenFile::new_cap(
            rnode,
            OpenFileFlags {
                read,
                write,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("open file")
    }

    fn make_dir_open_file(read: bool) -> Cap<OpenFile> {
        let rnode = RNode::new_cap(
            FsObjectId::new(3000),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
        )
        .expect("dir rnode");
        OpenFile::new_cap(
            rnode,
            OpenFileFlags {
                read,
                write: false,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )
        .expect("open file")
    }

    #[test]
    fn read_op_delegates_to_step_read() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(true, false);
        let mut buf = [0u8; 4];
        let mut op = OpenFileReadOp {
            file: &file,
            out: &mut buf,
            guard: &guard,
            cursor: 0,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Done(n) => {
                assert_eq!(n, 1);
                assert_eq!(buf[0], b'E');
            }
            other => panic!("expected Done(1), got {other:?}"),
        }
    }

    #[test]
    fn read_op_propagates_einval_when_not_readable() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(false, true);
        let mut buf = [0u8; 4];
        let mut op = OpenFileReadOp {
            file: &file,
            out: &mut buf,
            guard: &guard,
            cursor: 0,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Err(e) => assert_eq!(e, Errno::EINVAL),
            other => panic!("expected Err(EINVAL), got {other:?}"),
        }
    }

    #[test]
    fn write_op_delegates_to_step_write() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(false, true);
        let mut op = OpenFileWriteOp {
            file: &file,
            bytes: b"hello",
            guard: &guard,
            cursor: 0,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Done(n) => assert_eq!(n, 5),
            other => panic!("expected Done(5), got {other:?}"),
        }
    }

    #[test]
    fn write_op_propagates_einval_when_not_writable() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(true, false);
        let mut op = OpenFileWriteOp {
            file: &file,
            bytes: b"hi",
            guard: &guard,
            cursor: 0,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Err(e) => assert_eq!(e, Errno::EINVAL),
            other => panic!("expected Err(EINVAL), got {other:?}"),
        }
    }

    #[test]
    fn lseek_op_on_non_seekable_returns_espipe() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(true, false);
        let mut op = OpenFileLseekOp {
            file: &file,
            offset: 0,
            whence: 0,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Err(e) => assert_eq!(e, Errno::ESPIPE),
            other => panic!("expected Err(ESPIPE), got {other:?}"),
        }
    }

    #[test]
    fn lseek_op_on_directory_returns_eisdir() {
        let _g = setup();
        let guard = guard();
        let file = make_dir_open_file(true);
        let mut op = OpenFileLseekOp {
            file: &file,
            offset: 0,
            whence: 0,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Err(e) => assert_eq!(e, Errno::EISDIR),
            other => panic!("expected Err(EISDIR), got {other:?}"),
        }
    }

    #[test]
    fn ioctl_op_on_chardev_returns_enosys() {
        let _g = setup();
        let guard = guard();
        let file = make_char_open_file(true, true);
        // Construct an OpenFileIoctlCaller that does not require a real
        // process — `step_ioctl` short-circuits on chardev backings
        // before it consults the caller, so we can use a default-shaped
        // caller built from a freshly bootstrapped init process.
        crate::process::execution::reset_init_process_for_test();
        crate::process::structure::reset_pid_counter_for_test();
        crate::thread_runtime::structure::reset_tid_counter_for_test();
        let proc_cap = crate::process::bootstrap_init_process(
            crate::vm::AddressSpace::new_cap_for_platform::<crate::vm::TestPmap>().expect("aspace"),
        )
        .expect("init");
        let caller = OpenFileIoctlCaller::from_process(&proc_cap);
        let mut op = OpenFileIoctlOp {
            file: &file,
            caller,
            request: OpenFileIoctl::Tcgets,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Err(e) => assert_eq!(e, Errno::ENOSYS),
            other => panic!("expected Err(ENOSYS), got {other:?}"),
        }
    }

    #[test]
    fn ioctl_op_on_tty_returns_termios() {
        let _g = setup();
        let guard = guard();
        let file = make_tty_open_file(true, true, 7, "ttyS7-wraptest");
        crate::process::execution::reset_init_process_for_test();
        crate::process::structure::reset_pid_counter_for_test();
        crate::thread_runtime::structure::reset_tid_counter_for_test();
        let proc_cap = crate::process::bootstrap_init_process(
            crate::vm::AddressSpace::new_cap_for_platform::<crate::vm::TestPmap>().expect("aspace"),
        )
        .expect("init");
        let caller = OpenFileIoctlCaller::from_process(&proc_cap);
        let mut op = OpenFileIoctlOp {
            file: &file,
            caller,
            request: OpenFileIoctl::Tcgets,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<ProcessIdentity>::new();
        match op.step(&mut ctx) {
            V3::Done(OpenFileIoctlResult::Termios(_)) => {}
            other => panic!("expected Done(Termios), got {other:?}"),
        }
    }
}
