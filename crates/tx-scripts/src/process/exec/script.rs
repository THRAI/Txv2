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

use tx_hal::{EntropyIf, PmapIf, UserTrapContext};
use tx_subsystems::cred::{step_apply_suid_for_exec, Capability, Gid, Uid};
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{read_exact_at, PageContainer};
use tx_subsystems::process::{
    step_close_cloexec_fds, step_install_brk_for_exec, step_reset_signal_dispositions_for_exec,
    ProcessIdentity,
};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::{Credential, InodeMeta, OpenFileFlags, RNodeBacking};
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
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};

/// User page size — RV64 today; mirrors `vm::USER_PAGE_SIZE` so the
/// brk-base round-up doesn't require pulling in another import.
const USER_PAGE_SIZE: u64 = 4096;

/// Initial-read window over the ELF image used to cover the header and
/// program-header table. Sized at one page (4 KiB) which is well over
/// `Elf64_Ehdr` (64 bytes) + `MAX_PHDRS=64 * Elf64_Phdr=56` ≈ 3.6 KiB.
/// If a future image carries more program headers, the parser's
/// `MAX_PHDRS` cap rejects it before this read undershoots.
const INITIAL_PARSE_READ: usize = 4096;

/// Cumulative count of times the shebang (`#!`) handler fired.
pub static EXEC_SHEBANG_FIRED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Errno from the most recent `step_open` failure in `exec_script`.
pub static EXEC_LAST_OPEN_ERRNO: core::sync::atomic::AtomicI32 =
    core::sync::atomic::AtomicI32::new(0);

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
            // Wave 3 walker DAC predicate emits EACCES on
            // `WalkCause::TraverseDenied` and on `step_open` mode
            // checks; Wave 4 Part 5's pre-Phase-6 X-bit auth uses
            // EPERM-shape errors. Both map to the same exec-side
            // variant (Linux returns -EACCES for either).
            Errno::EACCES | Errno::EPERM => Self::PermissionDenied,
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

    /// Map an `ExecError` to the Linux RV64 generic-ABI `-errno` value
    /// the syscall arm writes back into the userspace `a0` register.
    /// Linux returns errors as negative magnitudes (e.g. `ENOENT = 2`
    /// becomes `-2`). Used by the Phase 6 NR_EXECVE arm in
    /// `tx_shims::linux_syscall::sys_execve`.
    ///
    /// The values match `tx_shims::linux_syscall::errno_to_i32` for
    /// consistency across the syscall surface.
    pub fn to_errno_i32(self) -> i32 {
        match self {
            // ENAMETOOLONG
            ExecError::PathTooLong => -36,
            // ENOENT
            ExecError::PathNotFound => -2,
            // ENOTDIR
            ExecError::NotADirectory => -20,
            // EACCES
            ExecError::PermissionDenied => -13,
            // ELOOP
            ExecError::SymlinkLoop => -40,
            // ENOEXEC
            ExecError::NotExecutable => -8,
            // EINVAL
            ExecError::InvalidArgument => -22,
            // ENOMEM
            ExecError::OutOfMemory => -12,
            // EBUSY
            ExecError::Busy => -16,
            // EIO
            ExecError::IoError => -5,
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
pub async fn exec_script<P: PmapIf + EntropyIf>(
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
    //
    // Walker discipline today: `step_walk` / `step_open` are `async fn`
    // but never reach an `.await` point internally — every backend in
    // tree resolves synchronously (the no-op `.await` comments at the
    // top of `vfs::walker` document this). To keep the resulting
    // `exec_script` future `Send` (the production thread future
    // submits it to the reactor's Send-bound `submit_task`) we poll
    // the walker future once with a noop waker rather than awaiting
    // it. The borrowed `&Guard` then only lives across the
    // synchronous poll, never across a suspension point. When ext4 /
    // page-cache backends grow real waits, this site shifts to the
    // canonical `take a fresh guard inside an await_*` shape per
    // `vm::execution::fault_script`.
    let openfile = {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let rooted_at = process.cwd().ok_or(ExecError::PathNotFound)?;
        let outcome = step_open(
            rooted_at,
            path,
            OpenFileFlags {
                read: true,
                write: false,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
            0,
            cred,
            &guard,
        );
        let result = match outcome {
            V3::Done(file) => Ok(file),
            V3::Continue { .. } | V3::Yield { .. } => Err(ExecError::Busy),
            V3::Err(err) => {
                EXEC_LAST_OPEN_ERRNO.store(err as i32, core::sync::atomic::Ordering::Relaxed);
                Err(ExecError::from_walker_errno(Errno::from(err)))
            }
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

    // ----- Phase 1 (cont) — execute-bit authorisation ----------------
    //
    // `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`.
    // Wave 3's `step_open` enforced the R/W bits implied by
    // `OpenFileFlags`; the *execute* check is exec-specific and was
    // deferred to here. The check mirrors Linux's `inode_permission(.,
    // MAY_EXEC)`: any-X-bit short-circuit for `CAP_DAC_OVERRIDE`
    // callers, otherwise the standard owner/group/other triplet on the
    // X bits. Closes LTP `execve02` (non-root cannot execute a 0o600
    // root-owned binary) and the EACCES sub-cases of `execve03`.
    let exec_meta = openfile.rnode().meta();
    check_exec_perm(&exec_meta, cred)?;

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
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let outcome = read_exact_at(&file_pc, 0, &mut header_bytes, &guard);
        let result = match outcome {
            V3::Done(()) => Ok(()),
            V3::Continue { .. } | V3::Yield { .. } => Err(ExecError::Busy),
            V3::Err(err) => Err(ExecError::from_read_errno(err.into())),
        };
        drop(guard);
        result?;
    }

    // ===== Phase 2.5 — shebang (#!) dispatch ===========================
    //
    // If the file starts with "#!" treat it as a script: parse the
    // interpreter path (and optional single argument) from the first
    // line, then re-invoke exec_script with the interpreter as the new
    // target.  Mirrors Linux binfmt_script.  One level of recursion is
    // sufficient (the interpreter itself must be a real ELF binary).
    if header_bytes.starts_with(b"#!") {
        if let Some((interp, opt_arg)) = shebang_parse(&header_bytes) {
            EXEC_SHEBANG_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Build new argv: [interp, opt_arg?, script_path, argv[1..]...]
            let mut new_argv: Vec<Vec<u8>> = Vec::new();
            new_argv.push(interp.to_vec());
            if let Some(arg) = opt_arg {
                new_argv.push(arg.to_vec());
            }
            new_argv.push(path.to_vec());
            for &a in argv.iter().skip(1) {
                new_argv.push(a.to_vec());
            }
            let interp_path: Vec<u8> = interp.to_vec();
            let new_argv_refs: Vec<&[u8]> = new_argv.iter().map(|v| v.as_slice()).collect();
            return alloc::boxed::Box::pin(exec_script::<P>(
                process,
                thread,
                &interp_path,
                &new_argv_refs,
                envp,
                cred,
            ))
            .await;
        }
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

    // ===== Phase 3.5 — apply S_ISUID / S_ISGID =======================
    //
    // `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`.
    // Per Open Q #2 (DECIDED 2026-05-06): the cred recompute lands
    // BETWEEN parse (Phase 3) and `build_aspace` (Phase 4). Cred
    // mutation is reversible (the helper returns `previous_cred` so a
    // caller could roll back via `step_setresuid` / `step_setresgid`
    // if a later pre-PoNR phase fails). Production exec_script does
    // not roll back — Linux's exec failure modes between Phase 3.5 and
    // Phase 6's `replace_aspace` PoNR leave the new cred installed
    // (see the slice plan's risk #5: `replace_aspace` is documented
    // infallible, so the only failure modes here are `OutOfMemory` /
    // `Busy` from Phase 4-5, which are equivalent to fork-then-fail
    // and not user-observable per LTP).
    //
    // The recompute MUST run before Phase 5's auxv build so the
    // `AT_SECURE` slot reflects the post-recompute effective-id delta.
    let exec_outcome = step_apply_suid_for_exec(
        process,
        Uid(exec_meta.uid),
        Gid(exec_meta.gid),
        exec_meta.mode,
    );
    let at_secure = exec_outcome.is_some_and(|o| o.at_secure);

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

    // ===== Phase 5a — eagerly populate partial-last-page bytes ========
    //
    // Per the ELF spec, bytes in the LAST file-backed page of a LOAD
    // segment that lie beyond `vaddr + filesz` must read as zero.
    // `register_load_segment` rounds the file-backed range DOWN to a
    // page boundary so the partial last page (if any) lands in the
    // segment's anon range. Here we eagerly populate that page's
    // file-content prefix from the PageContainer; the rest of the
    // page stays zero (fresh anon allocation).
    //
    // Without this, the partial-last-page bytes past `filesz` would
    // be either zero (if the recipe is anon — the case we set up here)
    // or arbitrary file bytes from the next file-offset region (the
    // case before the fix; busybox.musl crashed dereferencing
    // ".got\0ata" string fragments via gp-relative loads into its
    // BSS-extension area).
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    for segment in &image_plan.load_segments {
        if segment.filesz == 0 {
            continue;
        }
        // Only segments with BSS extension (memsz > filesz) get the
        // partial-last-page anon-with-eager-copy treatment per
        // `register_load_segment`'s comment block. Segments without
        // BSS keep the original Page-backed-up-to-`file_end_rounded`
        // shape — there's no observable junk past filesz because
        // userspace doesn't access bytes past `vaddr + memsz`.
        if segment.memsz <= segment.filesz {
            continue;
        }
        let file_end = segment
            .vaddr
            .checked_add(segment.filesz)
            .ok_or(ExecError::NotExecutable)?;
        let partial_in_page = file_end & (page_size - 1);
        if partial_in_page == 0 {
            continue;
        }
        let partial_start = file_end - partial_in_page;
        // File offset of `partial_start`. The segment's
        // `file_offset` corresponds to `vaddr`; offsetting by
        // `partial_start - vaddr` gives the file offset of the
        // partial-last-page's start.
        let file_off = segment
            .file_offset
            .checked_add(partial_start - segment.vaddr)
            .ok_or(ExecError::NotExecutable)?;
        let mut buf = alloc::vec![0u8; partial_in_page as usize];
        {
            use StepOutcome as V3;
            let guard = step_engine::guard();
            match read_exact_at(&segment.backing, file_off, &mut buf, &guard) {
                V3::Done(()) => {}
                V3::Continue { .. } | V3::Yield { .. } => return Err(ExecError::Busy),
                V3::Err(_) => return Err(ExecError::NotExecutable),
            }
        }
        match vm_scripts::populate_detached_user_range(&new_aspace, partial_start, &buf).await {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(err) => {
                return Err(ExecError::from_populate_errno(err.into()));
            }
            _ => return Err(ExecError::Busy),
        }
    }

    // ===== Phase 5 — compose + populate user stack ===================
    //
    // `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`. Build the
    // argv/envp/auxv image (pure kernel buffer; no aspace coupling
    // yet) and write it into the freshly built detached aspace's
    // anonymous-private stack range via V2's pre-publish lane.
    // Snapshot the caller's cred so musl's `__init_security` can read
    // `at_uid` / `at_euid` / `at_gid` / `at_egid` straight off the
    // auxv stack image. `process.cred()` returns `None` only for a
    // zombie target; the exec front-end keeps us alive through this
    // point, so the `Cred::root()` fallback is purely defensive.
    let cred = process
        .cred()
        .unwrap_or_else(tx_subsystems::cred::Cred::root);

    // CSPRNG chore (chore/csprng-at-random): pull 16 bytes from
    // the platform's `EntropyIf` impl (RV64: rdtime + xorshift
    // counter; other boards: deterministic counter default) to
    // seed musl's stack canary via the AT_RANDOM auxv slot. Not a
    // real CSPRNG, but materially stronger than the previous
    // static `[0; 16]` and adequate for txKernel's current trust
    // model (no ASLR, no untrusted input).
    let mut at_random_bytes = [0u8; 16];
    <P as EntropyIf>::fill_random(&mut at_random_bytes);

    let auxv_facts = AuxvFacts {
        at_phdr: parsed.at_phdr,
        at_phent: ELF64_PHENT,
        at_phnum: parsed.at_phnum,
        at_pagesz: USER_PAGE_SIZE,
        // Drift-cleanup chore (2026-05-07): `AT_BASE = 0` for the
        // v1 static-`ET_EXEC` contract (no PT_INTERP per
        // `EXEC_v1.md`'s static-only pin); a future dynamic-link
        // slice flips this to the interpreter's load bias.
        // `AT_ENTRY` carries the parsed ELF entry through to musl's
        // `__libc_start_main`.
        at_base: 0,
        at_entry: parsed.entry,
        at_uid: cred.uid.raw() as u64,
        at_euid: cred.euid.raw() as u64,
        at_gid: cred.gid.raw() as u64,
        at_egid: cred.egid.raw() as u64,
        // Wave 4 Part 5: post-Phase-3.5 effective-id delta. `1` when
        // the binary's S_ISUID bit changed the effective uid, or its
        // S_ISGID-with-group-X bit changed the effective gid. Per Q5
        // (DECIDED 2026-05-06) the rule is short-form — file
        // capabilities and `nosuid` mounts (future slices) will
        // refine.
        at_secure: if at_secure { 1 } else { 0 },
        at_random_bytes,
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
        StepOutcome::Done(()) => {}
        StepOutcome::Err(err) => {
            return Err(ExecError::from_populate_errno(err.into()));
        }
        _ => return Err(ExecError::Busy),
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
    // Arch-specific SP register (x2 on RV64, x3 on LA64).
    regs[initial_user_sp_reg()] = sp;
    UserTrapContext {
        regs,
        pc,
        status: 0,
        fp: tx_hal::UserFpContext::empty(),
    }
}

/// Translate the parser's segment-flag shape (`bool`-named fields) to
/// the vm-scripts shape (different field names).
pub(crate) const fn initial_user_sp_reg_for_arch(arch: tx_hal::Arch) -> usize {
    match arch {
        tx_hal::Arch::Riscv64 => 2,
        tx_hal::Arch::LoongArch64 => 3,
    }
}

fn initial_user_sp_reg() -> usize {
    initial_user_sp_reg_for_arch(tx_hal::Arch::Riscv64)
}

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

/// Poll an `async` walker future synchronously, panicking if it
/// returns `Pending`.
///
/// `step_walk` and `step_open` are `async fn` but never reach an
/// `.await` point in the in-tree backends today (the walker module
/// docs at `crates/tx-subsystems/src/vfs/walker.rs:23` say "every
/// `.await` is a no-op today"). Driving them through `.await` from
/// `exec_script` would capture the borrowed `&Guard` across the
/// suspension point — making the resulting `exec_script` future
/// `!Send` because `Guard` is deliberately `!Send + !Sync`.
///
/// Polling once with a noop waker resolves immediately for every
/// in-tree walker path, and the borrowed `&Guard` only lives across
/// the synchronous poll body — not across any suspension point. When
/// ext4 / page-cache backends grow real waits (and `step_walk`
/// actually returns `Pending`), this helper's `panic!` arm fires
/// and the call site must shift to the canonical "fresh guard inside
/// `await_*`" shape per `vm::execution::fault_script`.
fn poll_walker_synchronously<F: core::future::Future>(future: F) -> F::Output {
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    // Noop waker: cloning yields another noop waker; wake / wake_by_ref
    // are no-ops; drop is a no-op.
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: the vtable above never dereferences the data pointer.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = pin!(future);
    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!(
            "poll_walker_synchronously: walker returned Pending; in-tree walker \
             backends never await today (cite: vfs::walker module docs). When \
             real-await backends land this site must use the fresh-guard-inside-\
             await_* pattern instead."
        ),
    }
}

/// Pre-Phase-6 execute-bit authorisation. Mirrors Linux's
/// `inode_permission(., MAY_EXEC)`: callers with `CAP_DAC_OVERRIDE`
/// short-circuit IF the binary has at least one X bit set anywhere
/// (the POSIX exception that root cannot execute a file with no X bits
/// — Linux honours this; LTP `execve03` checks). Otherwise the
/// standard owner / group / other triplet on the X bits decides.
///
/// Returns `Err(ExecError::PermissionDenied)` (→ `-EACCES` at the
/// syscall arm) on denial. The slice does not yet model
/// `CAP_DAC_READ_SEARCH` (no LTP test gates on it).
///
/// Cites: `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`,
/// `txdoc:VFS-CHECKS-PERMISSIONS-1`.
fn check_exec_perm(meta: &InodeMeta, cred: &Credential) -> Result<(), ExecError> {
    let mode = meta.mode as u32;
    if cred.effective_caps.contains(Capability::DAC_OVERRIDE) {
        return Ok(());
    }
    let bits = if cred.uid == meta.uid {
        (mode >> 6) & 0o7
    } else if cred.gid == meta.gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    if bits & 0o1 == 0 {
        return Err(ExecError::PermissionDenied);
    }
    Ok(())
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

/// Parse a `#!` shebang line from the start of a file's header bytes.
/// Returns `(interpreter_path, optional_arg)` slices into `header`, or
/// `None` if the line is malformed (empty interpreter path).
///
/// Format: `#! <whitespace>? <interp> <whitespace> <opt_arg>? <newline>`
/// Only the first argument after the interpreter is captured (Linux
/// binfmt_script passes at most one optional argument).
fn shebang_parse(header: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    debug_assert!(header.starts_with(b"#!"));
    let line_end = header[2..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| i + 2)
        .unwrap_or(header.len());
    let line = shebang_trim_start(&header[2..line_end]);
    if line.is_empty() {
        return None;
    }
    let (interp, rest) = shebang_split_word(line);
    let interp = shebang_trim_end(interp);
    if interp.is_empty() {
        return None;
    }
    let rest = shebang_trim_start(rest);
    let opt_arg = if rest.is_empty() {
        None
    } else {
        let (arg, _) = shebang_split_word(rest);
        let arg = shebang_trim_end(arg);
        if arg.is_empty() {
            None
        } else {
            Some(arg)
        }
    };
    Some((interp, opt_arg))
}

fn shebang_trim_start(s: &[u8]) -> &[u8] {
    let i = s
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(s.len());
    &s[i..]
}

fn shebang_trim_end(s: &[u8]) -> &[u8] {
    let i = s
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t' && b != b'\r')
        .map(|i| i + 1)
        .unwrap_or(0);
    &s[..i]
}

fn shebang_split_word(s: &[u8]) -> (&[u8], &[u8]) {
    let i = s
        .iter()
        .position(|&b| b == b' ' || b == b'\t')
        .unwrap_or(s.len());
    (&s[..i], &s[i..])
}

#[cfg(test)]
mod tests;
