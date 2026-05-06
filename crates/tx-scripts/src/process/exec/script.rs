//! Exec script orchestration (Phase 5 of the ELF-loader plan).
//!
//! Realises the eight-phase EXEC_v1 protocol against the seams shipped
//! by Wave 1 (`vm::scripts`, `page_backed::read_exact_at`) and Wave 2
//! (`step_close_cloexec_fds`, `step_reset_signal_dispositions_for_exec`,
//! `step_install_brk_for_exec`). The script integrates the loader's
//! ELF parser (`super::loader::parse_image_plan`) and stack-image
//! builder (`super::stack::build_initial_user_stack`) with the VFS
//! walker (`vfs::walker::step_open`) so a `path` resolves all the way
//! to a fully populated detached `AddressSpace` ready for the
//! Phase-6 atomic swap.
//!
//! Doc anchors:
//! - `txdoc:EXEC-7-EIGHT-PHASES`
//! - `txdoc:EXEC-8-1-RESOLVE-AND-OPEN`
//! - `txdoc:EXEC-8-3-PARSE-AND-VALIDATE`
//! - `txdoc:EXEC-9-1-OPEN-AND-PLAN-THE-IMAGE`
//! - `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`
//! - `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`
//! - `txdoc:EXEC-10-COLLAPSE-OLD-AS-WORK`
//! - `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`
//! - `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`
//! - `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC`
//! - `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`
//! - `txdoc:EXEC-12-4-INSTALL-BRK`
//! - `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT`
//! - `txdoc:THREAD-5-1-STATE-PLACEMENT`
//!
//! ## PoNR discipline
//!
//! Phases 1-5 are reversible: any error returns `Err(ExecError::*)`
//! and the freshly-built detached `Cap<AddressSpace>` (if any) drops
//! locally — its `Drop` reclaims memory under EBR with no observable
//! side-effects on the caller's process.
//!
//! Phase 6 is the irreversible visibility boundary: a single
//! `process.replace_aspace(new_aspace)` followed by a single
//! `thread.payload().store_saved_user_context(...)`. Both atomic, both
//! infallible. After this point the function performs only Phase-7
//! commits (Wave 2 helpers, all infallible by EXEC-PONR) and returns
//! `Ok(())`. There is no `?`, no `.await`, and no fallible call
//! between the Phase-6 swap and the function's return.

use alloc::vec::Vec;

use tx_hal::{PmapIf, UserTrapContext};
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::page_backed::{read_exact_at, PageContainer};
use tx_subsystems::process::{
    step_close_cloexec_fds, step_install_brk_for_exec, step_reset_signal_dispositions_for_exec,
    ProcessIdentity,
};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::{Credential, OpenFileFlags, RNodeBacking};
use tx_subsystems::vfs::walker::step_open;
use tx_subsystems::vm::scripts::{
    self as vm_scripts, BssTail as VmBssTail, ImagePlan as VmImagePlan,
    LoadSegment as VmLoadSegment, SegmentFlags as VmSegmentFlags, USER_STACK_TOP_DEFAULT,
};

use super::loader::{
    parse_image_plan, ExecImagePlan, LoadSegment as ParsedLoadSegment, ParseError,
    SegmentFlags as ParsedSegmentFlags, ELF64_PHENT,
};
use super::stack::{build_initial_user_stack, AuxvFacts};

/// User page size — RV64 today; mirrors `vm::USER_PAGE_SIZE` so the
/// brk-base round-up doesn't require pulling in another import.
const USER_PAGE_SIZE: u64 = 4096;

/// Initial-read window over the ELF image used to cover the header and
/// program-header table. Sized at one page (4 KiB) which is well over
/// `Elf64_Ehdr` (64 bytes) + `MAX_PHDRS=64 * Elf64_Phdr=56` ≈ 3.6 KiB.
/// If a future image carries more program headers, the parser's
/// `MAX_PHDRS` cap rejects it before this read undershoots.
const INITIAL_PARSE_READ: usize = 4096;

/// Linux-flavoured exec errors. Mapped to `-errno` by the syscall arm
/// (Phase 6 of the ELF-loader plan, out of scope here).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExecError {
    /// `ENAMETOOLONG`. Path component or full path too long.
    PathTooLong,
    /// `ENOENT`. Some component of `path` does not exist.
    PathNotFound,
    /// `ENOTDIR`. Non-directory in the middle of a walk.
    NotADirectory,
    /// `EACCES`. Caller lacks permission to traverse / read / execute
    /// some component. Day-1 walker is permissive; this is reserved.
    PermissionDenied,
    /// `ELOOP`. Symlink-loop budget exceeded during path resolution.
    SymlinkLoop,
    /// `ENOEXEC`. File is not a recognised RV64 ELF executable
    /// (header / phdr validation failed, or backend reads short).
    NotExecutable,
    /// `EINVAL`. Invalid argument shape (e.g. plan that would
    /// underflow the user stack arithmetic).
    InvalidArgument,
    /// `ENOMEM`. Page-allocator / zone-allocator pressure. The exec
    /// path drops every reservation it acquired before this fired.
    OutOfMemory,
    /// `EBUSY`. Range-lock contention or unexpected pmap publish
    /// stall on a detached aspace; effectively unreachable for the v1
    /// surface and mostly a placeholder for tests.
    Busy,
    /// `EIO`. Page-cache / direct-map I/O failure.
    IoError,
}

impl ExecError {
    fn from_walker_errno(err: Errno) -> Self {
        match err {
            Errno::ENOENT => Self::PathNotFound,
            Errno::ENOTDIR => Self::NotADirectory,
            Errno::ENAMETOOLONG => Self::PathTooLong,
            Errno::ELOOP => Self::SymlinkLoop,
            Errno::EPERM => Self::PermissionDenied,
            Errno::ENOMEM => Self::OutOfMemory,
            Errno::EIO => Self::IoError,
            Errno::ENODEV | Errno::ENOSYS => Self::NotExecutable,
            _ => Self::InvalidArgument,
        }
    }

    fn from_read_errno(err: Errno) -> Self {
        match err {
            Errno::ENOEXEC => Self::NotExecutable,
            Errno::ENOMEM => Self::OutOfMemory,
            Errno::EIO => Self::IoError,
            _ => Self::InvalidArgument,
        }
    }

    fn from_populate_errno(err: Errno) -> Self {
        match err {
            Errno::ENOMEM => Self::OutOfMemory,
            Errno::EFAULT | Errno::EINVAL => Self::InvalidArgument,
            Errno::EBUSY => Self::Busy,
            Errno::EIO => Self::IoError,
            _ => Self::InvalidArgument,
        }
    }

    fn from_parse_error(err: ParseError) -> Self {
        // Every parser failure mode maps to ENOEXEC — userspace cannot
        // tell `Magic` from `Phdr` from `LoadSegment`. The parser keeps
        // them distinguished for kernel-side tracing only.
        let _ = err;
        Self::NotExecutable
    }

    fn from_build_aspace_error(err: vm_scripts::ScriptError) -> Self {
        match err {
            vm_scripts::ScriptError::InvalidImage => Self::NotExecutable,
            vm_scripts::ScriptError::OutOfMemory => Self::OutOfMemory,
            vm_scripts::ScriptError::WouldBlock => Self::Busy,
            vm_scripts::ScriptError::Efault => Self::InvalidArgument,
            vm_scripts::ScriptError::Io => Self::IoError,
            vm_scripts::ScriptError::Map(_) | vm_scripts::ScriptError::Pmap(_) => Self::OutOfMemory,
        }
    }
}

/// Replace `process`'s active address space with a fresh image loaded
/// from `path` and seed the calling `thread`'s saved user-trap context
/// with the new entry point and stack pointer.
///
/// On `Ok(())` the eight-phase EXEC_v1 protocol has run to completion:
/// the new aspace is observable to every concurrent VM lookup, the
/// thread's `saved_user_context` carries the new image's `e_entry` /
/// `initial_sp`, and the per-frame Phase-7 commits (CLOEXEC sweep,
/// signal disposition reset, brk install) have been applied. The
/// caller (the NR_EXECVE syscall arm, Phase 6 of the ELF-loader plan)
/// MUST NOT write a syscall return for this thread — the next
/// userspace re-entry, driven by the production reactor loop's
/// `enter_userspace_with_context`, picks up the fresh
/// `saved_user_context` and runs the new program.
///
/// On `Err(_)`, every reservation acquired by the script has dropped:
/// `process.aspace_cap()` returns the same `Cap` it did on entry, and
/// the syscall arm encodes the error as `-errno` via the standard
/// `ExecError → Linux errno` mapping.
///
/// Cites: `txdoc:EXEC-7-EIGHT-PHASES`,
/// `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT`.
pub async fn exec_script<P: PmapIf>(
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    cred: &Credential,
) -> Result<(), ExecError> {
    // ===== Phase 1 — resolve path + open file =========================
    //
    // `txdoc:EXEC-8-1-RESOLVE-AND-OPEN`. Snapshot a fresh epoch guard,
    // resolve `path` via the VFS walker, then drop the guard before
    // any subsequent `.await` site (V1 / V2 take fresh guards
    // internally; nesting would violate
    // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
    let openfile = {
        let guard = tx_substrate::epoch::guard();
        let rooted_at = process.cwd().ok_or(ExecError::PathNotFound)?;
        let outcome = step_open(
            rooted_at,
            path,
            OpenFileFlags {
                read: true,
                write: false,
                append: false,
                cloexec: false,
            },
            0,
            cred,
            &guard,
        )
        .await;
        let result = match outcome {
            StepOutcome::Done(file) | StepOutcome::Advanced(file) => Ok(file),
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                Err(ExecError::Busy)
            }
            StepOutcome::Err(err) => Err(ExecError::from_walker_errno(err)),
        };
        drop(guard);
        result?
    };

    // Snapshot the file's `Cap<PageContainer>` once. The exec image's
    // PageContainer lives behind the resolved RNode's
    // `RNodeBacking::PageBacked { pc }` (tmpfs's regular files use
    // `PageContainerKind::Anon { Reclaimable }` per pre-ELF Phase 3b;
    // ext4 will plug in a `PageContainerKind::File { mount, fs_object_id }`
    // shape via a future `materialise_rnode` hook). Other backings —
    // directories, symlinks (chased by the walker, never terminal
    // here), TTY struct-payloads, projection rows — are not
    // executable.
    let file_pc = match openfile.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return Err(ExecError::NotExecutable),
    };
    let file_size = file_pc.size_bytes();

    // ===== Phase 2 — read header + program headers ===================
    //
    // `txdoc:EXEC-8-3-PARSE-AND-VALIDATE`. One bounded targeted read
    // over the file's PageContainer. `read_exact_at` enforces the
    // short-read contract (EOF before fill → `Errno::ENOEXEC`). The
    // window covers ELF64 header (64 B) + the parser's MAX_PHDRS=64
    // worth of program headers (≈ 3.6 KiB), comfortably within one
    // 4 KiB page.
    let read_len = core::cmp::min(file_size as usize, INITIAL_PARSE_READ);
    if read_len < 64 {
        // ELF64 header alone is 64 bytes — anything smaller cannot be
        // a valid binary.
        return Err(ExecError::NotExecutable);
    }
    let mut header_bytes: Vec<u8> = alloc::vec![0u8; read_len];
    {
        let guard = tx_substrate::epoch::guard();
        let outcome = read_exact_at(&file_pc, 0, &mut header_bytes, &guard);
        let result = match outcome {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => Ok(()),
            StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
                Err(ExecError::Busy)
            }
            StepOutcome::Err(err) => Err(ExecError::from_read_errno(err)),
        };
        drop(guard);
        result?;
    }

    // ===== Phase 3 — parse + validate (pure CPU) =====================
    //
    // `txdoc:EXEC-8-3-PARSE-AND-VALIDATE`. Goblin-backed parser owns
    // every header and program-header check (class / data / version /
    // arch / type / no PT_INTERP / no PT_DYNAMIC / phdr-table fits /
    // congruence / overlap / W^X). All failures collapse to
    // `ExecError::NotExecutable` at the syscall boundary.
    let parsed: ExecImagePlan =
        parse_image_plan(&header_bytes).map_err(ExecError::from_parse_error)?;

    // Compose the brk base from the image plan: the page-rounded end
    // of the highest LOAD segment's memory footprint. Linux's
    // `setup_arg_pages` does the same — heap starts where the image
    // ends so static binaries can grow up. Computed here so Phase 7
    // is a pure infallible store.
    let new_brk_base = compute_brk_base(&parsed.load_segments)?;

    // ===== Phase 4 — build detached AddressSpace =====================
    //
    // `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`. Bridge the
    // parser's `LoadSegment` (which has no PageContainer — the
    // parser is purely byte-driven) to `vm::scripts::LoadSegment`
    // (which carries the `Cap<PageContainer>` the recipe row references
    // for demand-faulting bytes from the file). Every LOAD segment
    // shares the same backing `Cap` (they all view different ranges
    // of the same file).
    let image_plan = build_vm_image_plan(&parsed, &file_pc);
    // V1 (`build_aspace_from_image`) takes no `&Guard` — it acquires
    // its own per-call guards internally. Per
    // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` callers must NOT
    // hold a guard at the call site.
    let new_aspace = vm_scripts::build_aspace_from_image::<P>(&image_plan)
        .map_err(ExecError::from_build_aspace_error)?;

    // ===== Phase 5 — compose + populate user stack ===================
    //
    // `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`. Build the
    // argv/envp/auxv image (pure kernel buffer; no aspace coupling
    // yet) and write it into the freshly built detached aspace's
    // anonymous-private stack range via V2's pre-publish lane.
    let auxv_facts = AuxvFacts {
        at_phdr: parsed.at_phdr,
        at_phent: ELF64_PHENT,
        at_phnum: parsed.at_phnum,
        at_pagesz: USER_PAGE_SIZE,
    };
    let stack_image = build_initial_user_stack(USER_STACK_TOP_DEFAULT, argv, envp, &auxv_facts);

    // V2 (`populate_detached_user_range`) also acquires its own
    // guards internally; same discipline as V1.
    match vm_scripts::populate_detached_user_range(
        &new_aspace,
        stack_image.initial_sp,
        &stack_image.bytes,
    )
    .await
    {
        StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
        StepOutcome::AdvancedThenBlocked(_, _) | StepOutcome::Blocked(_) => {
            return Err(ExecError::Busy);
        }
        StepOutcome::Err(err) => return Err(ExecError::from_populate_errno(err)),
    }

    // ----- Phase 5 (cont) — collapse old-AS work -------------------
    //
    // `txdoc:EXEC-10-COLLAPSE-OLD-AS-WORK`. No-op for the v1 slice:
    // no `CLONE_FILES` / `CLONE_SIGHAND` exists, the fd table and
    // sig_actions are owned in-place by `process`, and there are no
    // sibling threads to zombify. The Phase-7 commit list below
    // mutates the in-place state directly. Spec anchor pinned for
    // when CLONE_* support arrives.

    // ===== Phase 6 — address-space visibility boundary ===============
    //
    // `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
    //
    // EXEC-PONR begins here. Two atomic stores, both infallible:
    //
    //   1. `process.replace_aspace(new_aspace)` — exchanges the
    //      `AtomicSlot<Cap<AddressSpace>>` and returns the previous
    //      `Cap` for EBR-deferred drop. The new aspace is observable
    //      to every concurrent fault / VM lookup the moment this
    //      store completes; the previous aspace's `Drop` runs after
    //      the Cap goes out of scope (end of this function), and
    //      EBR defers its reclamation past the current epoch per
    //      ZONE / EBR rules.
    //   2. `thread.payload().store_saved_user_context(...)` — seeds
    //      the calling thread's per-frame trap context with the new
    //      image's entry-point + stack pointer. The next iteration
    //      of `run_thread::<P>` reads this slot via the
    //      userspace-entry checkpoint (per
    //      `txdoc:THREAD-5-1-STATE-PLACEMENT`) and resumes via
    //      `enter_userspace_with_context`.
    //
    // Past this point the function performs only Phase-7 commits
    // (Wave 2 helpers, all infallible). No `?`, no `.await`, no
    // fallible call — the EXEC-PONR invariant is encoded in the
    // function's structure: every `?`-bearing or async call lives
    // in phases 1-5; phases 6-7 are a straight-line block of
    // synchronous, infallible operations.
    let _previous_aspace = process.replace_aspace(new_aspace);
    // `_previous_aspace` drops at end of scope; EBR defers reclamation
    // so any concurrent reader on another hart can finish its
    // observation of the old aspace before its memory is reused.

    let entry_pc = parsed.entry as usize;
    let initial_sp = stack_image.initial_sp as usize;
    let user_ctx = make_initial_user_trap_context(entry_pc, initial_sp);
    if let Some(payload) = thread.payload_cap() {
        payload.store_saved_user_context(Some(user_ctx));
    }

    // ===== Phase 7 — install per-frame replacements ==================
    //
    // `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT` (handled by Phase 6
    // above), `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC`,
    // `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`,
    // `txdoc:EXEC-12-4-INSTALL-BRK`. All Wave 2 helpers; all infallible
    // synchronous. Order is documented but immaterial — each commit
    // touches a disjoint slot.
    step_close_cloexec_fds(process);
    step_reset_signal_dispositions_for_exec(process);
    step_install_brk_for_exec(process, new_brk_base);

    // ===== Phase 8 — userspace re-entry ==============================
    //
    // The script does not directly re-enter userspace. The next
    // iteration of the production reactor's `run_thread::<P>` loop
    // observes `saved_user_context` populated above and dispatches
    // through `enter_userspace_with_context`. The caller (the future
    // NR_EXECVE syscall arm, Phase 6 of the ELF-loader plan) treats
    // `Ok(())` as "exec committed; do NOT write a syscall return for
    // this thread."
    Ok(())
}

/// Compose a fresh `UserTrapContext` for the new image's first
/// userspace entry. RV64 register file is zeroed (per System V psABI:
/// `_start` reads its arguments off the stack, not registers); only
/// `pc`, `sp` (= `regs[2]`), and `status` are seeded here.
///
/// `status` is left at zero — the platform's
/// `restore_user_context` decides how to compose `sstatus` for the
/// fresh image (typically a U-mode entry with interrupts enabled).
fn make_initial_user_trap_context(pc: usize, sp: usize) -> UserTrapContext {
    let mut regs = [0usize; 32];
    // RV64 SP is x2 per the integer-register assignments in the ABI.
    regs[2] = sp;
    UserTrapContext {
        regs,
        pc,
        status: 0,
    }
}

/// Translate the parser's segment-flag shape (`bool`-named fields) to
/// the vm-scripts shape (different field names).
const fn translate_flags(parsed: ParsedSegmentFlags) -> VmSegmentFlags {
    VmSegmentFlags {
        read: parsed.readable,
        write: parsed.writable,
        execute: parsed.executable,
    }
}

/// Bridge `parse_image_plan`'s view (no PageContainer) to the
/// vm-scripts view (LOAD segments carry the file's `Cap<PageContainer>`).
fn build_vm_image_plan(parsed: &ExecImagePlan, file_pc: &Cap<PageContainer>) -> VmImagePlan {
    let load_segments: alloc::vec::Vec<VmLoadSegment> = parsed
        .load_segments
        .iter()
        .map(|seg: &ParsedLoadSegment| VmLoadSegment {
            vaddr: seg.vaddr,
            memsz: seg.memsz,
            filesz: seg.filesz,
            file_offset: seg.file_offset,
            flags: translate_flags(seg.flags),
            backing: file_pc.clone(),
        })
        .collect();

    let bss_extension = parsed.bss_extension.map(|tail| VmBssTail {
        vaddr: tail.vaddr,
        size: tail.size,
    });

    VmImagePlan {
        entry: parsed.entry,
        stack_top: USER_STACK_TOP_DEFAULT,
        load_segments,
        bss_extension,
    }
}

/// Compute the page-rounded brk base from the parsed LOAD segments.
///
/// The brk region anchors at the highest `vaddr + memsz` of any LOAD
/// segment, page-rounded up. Static binaries' `_end` symbol matches
/// this position; subsequent `brk(2)` syscalls grow the heap on
/// demand. Returns `Err(ExecError::NotExecutable)` if every segment
/// has zero memsz (degenerate; loader rejects this earlier) or if
/// page-round overflows.
fn compute_brk_base(load_segments: &[ParsedLoadSegment]) -> Result<u64, ExecError> {
    let mut highest: u64 = 0;
    for seg in load_segments {
        let end = seg
            .vaddr
            .checked_add(seg.memsz)
            .ok_or(ExecError::NotExecutable)?;
        if end > highest {
            highest = end;
        }
    }
    if highest == 0 {
        return Err(ExecError::NotExecutable);
    }
    page_round_up(highest).ok_or(ExecError::NotExecutable)
}

/// Round `value` up to the next multiple of `USER_PAGE_SIZE`. Returns
/// `None` on overflow.
const fn page_round_up(value: u64) -> Option<u64> {
    let mask = USER_PAGE_SIZE - 1;
    match value.checked_add(mask) {
        Some(rounded) => Some(rounded & !mask),
        None => None,
    }
}

// Note: the original plan sketched `Result<core::convert::Infallible,
// ExecError>` so the type system would witness post-PoNR
// irrevocability. We picked `Result<(), ExecError>` instead — the
// type's `Ok(())` arm is uninhabitable in practice (the script's
// straight-line phase-7 block runs to completion or violates EXEC-PONR
// by panicking), and the syscall arm's "do not write a syscall return
// value" decision is made structurally by Phase 6 of the ELF-loader
// plan rather than encoded in this function's return type. See the
// report accompanying Phase 5.

#[cfg(test)]
mod tests;
