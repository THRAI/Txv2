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
use tx_subsystems::mount::MountFlags;
use tx_subsystems::page_backed::{read_exact_at, PageContainer};
use tx_subsystems::process::adapter::wait_routing::Mask;
use tx_subsystems::process::{
    step_close_cloexec_fds, step_install_brk_for_exec, step_reset_signal_dispositions_for_exec,
    ProcessIdentity,
};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::walker::step_open;
use tx_subsystems::vm::scripts::{
    self as vm_scripts, BssTail as VmBssTail, ImagePlan as VmImagePlan,
    LoadSegment as VmLoadSegment, SegmentFlags as VmSegmentFlags, INTERP_LOAD_BIAS_DEFAULT,
    USER_STACK_TOP_DEFAULT,
};

use super::loader::{
    parse_image_plan, parse_interp_plan, ExecImagePlan, LoadSegment as ParsedLoadSegment,
    ParseError, SegmentFlags as ParsedSegmentFlags, ELF64_PHENT, ET_DYN_LOAD_BIAS,
};
use super::stack::{build_initial_user_stack, AuxvFacts};
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use crate::adapter::vfs_exec::{
    Credential, DEntry, InodeKind, InodeMeta, OpenFileFlags, RNodeBacking,
};

/// User page size — RV64 today; mirrors `vm::USER_PAGE_SIZE` so the
/// brk-base round-up doesn't require pulling in another import.
const USER_PAGE_SIZE: u64 = 4096;

// ASLR functions moved inline to exec_script_inner

fn namespace_root_for_dentry(mut cursor: Cap<DEntry>) -> Cap<DEntry> {
    while let Some(parent) = cursor.parent_hint() {
        cursor = parent;
    }
    cursor
}

fn exec_root_for_path(cwd: &Cap<DEntry>, path: &[u8]) -> Cap<DEntry> {
    if path.starts_with(b"/") {
        namespace_root_for_dentry(cwd.clone())
    } else {
        cwd.clone()
    }
}

/// Emit a OBS-V1 §15.7 ProcessLabel Instant mapping `pid` to the PCB
/// `comm` just committed by `step_store_exec_identity`-equivalent
/// logic above. The daemon caches `pid → comm` and uses it as the
/// per-process Perfetto track's `ProcessDescriptor.process_name`, so
/// the timeline shows real program names (`busybox`, `basic_exec`)
/// for processes that exec'd into a new binary mid-trace.
///
/// No-op when no `HartEmitter` is installed on this hart (host
/// tests, boards without an observation ring) — same fallback
/// shape as the kernel's `init.rs::emit_process_label`.
fn emit_process_label_for(pid_low: u32, comm: &[u8; 16]) {
    let Some(em) = tx_observe::current() else {
        return;
    };
    let mut truncated = [0u8; 12];
    let n = core::cmp::min(comm.len(), truncated.len());
    truncated[..n].copy_from_slice(&comm[..n]);
    if !truncated.contains(&0) {
        truncated[truncated.len() - 1] = 0;
    }
    let payload = tx_observe::PayloadProcessLabel {
        process_id_low: pid_low,
        comm: truncated,
    };
    let (enc, len) = tx_observe::encode::encode_process_label(&payload);
    em.instant(
        tx_observe::TxTraceLevel::Sched,
        // Same `0x9000_0000 | pid` namespace as
        // `tx-kernel::init::emit_process_label` so the daemon's
        // dedupe sees both submit-time and post-exec re-emits as the
        // same logical event.
        tx_observe::EventNameId::from_raw(0x9000_0000 | pid_low),
        tx_observe::current_parent_span(),
        tx_observe::encode::process_label_tag(),
        &enc[..len as usize],
    );
}

/// Companion to [`emit_process_label_for`] — emits the OBS-V1 §15.8
/// PCB-group bundle (`pid → pgid + sid`) so the daemon can parent
/// the per-process Perfetto track under a pgrp swimlane. Called
/// from exec_script's post-PoNR `comm` commit alongside the
/// re-emit of [`PayloadProcessLabel`]; the kernel submit-time
/// site emits both via the `tx-kernel::init::emit_process_group`
/// twin.
fn emit_process_group_for(pid_low: u32, pgid_low: u32, sid_low: u32) {
    let Some(em) = tx_observe::current() else {
        return;
    };
    let payload = tx_observe::PayloadProcessGroup {
        process_id_low: pid_low,
        pgid_low,
        sid_low,
        _pad: 0,
    };
    let (enc, len) = tx_observe::encode::encode_process_group(&payload);
    em.instant(
        tx_observe::TxTraceLevel::Sched,
        // Same `0xA000_0000 | pid` namespace as
        // `tx-kernel::init::emit_process_group`.
        tx_observe::EventNameId::from_raw(0xA000_0000 | pid_low),
        tx_observe::current_parent_span(),
        tx_observe::encode::process_group_tag(),
        &enc[..len as usize],
    );
}

/// Initial-read window over the ELF image used to cover the header and
/// program-header table. Sized at one page (4 KiB) which is well over
/// `Elf64_Ehdr` (64 bytes) + `MAX_PHDRS=64 * Elf64_Phdr=56` ≈ 3.6 KiB.
/// If a future image carries more program headers, the parser's
/// `MAX_PHDRS` cap rejects it before this read undershoots.
const INITIAL_PARSE_READ: usize = 4096;

/// Cumulative count of times the shebang (`#!`) handler fired.
pub static EXEC_SHEBANG_FIRED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Maximum shebang recursion depth (Linux default: 4).
const SHEBANG_MAX_DEPTH: usize = 4;

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
pub async fn exec_script<P: PmapIf + EntropyIf + tx_hal::AuxvIf>(
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    cred: &Credential,
) -> Result<(), ExecError> {
    exec_script_inner::<P>(0, process, thread, path, argv, envp, cred).await
}

async fn exec_script_inner<P: PmapIf + EntropyIf + tx_hal::AuxvIf>(
    depth: usize,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    cred: &Credential,
) -> Result<(), ExecError> {
    // Shebang recursion guard (Linux limit: 4).
    if depth > SHEBANG_MAX_DEPTH {
        return Err(ExecError::IoError); // maps to ELOOP
    }

    // ---- ASLR helpers (inline closures) ----------------------------
    let randomize_et_dyn_base = || -> u64 {
        let mut buf = [0u8; 8];
        tx_services::random::fill_bytes(&mut buf);
        let r = u64::from_le_bytes(buf);
        let offset = r & ((1 << 24) - 1) & !(USER_PAGE_SIZE - 1);
        ET_DYN_LOAD_BIAS + offset
    };
    let randomize_stack_top = || -> u64 {
        let mut buf = [0u8; 8];
        tx_services::random::fill_bytes(&mut buf);
        (USER_STACK_TOP_DEFAULT + (u64::from_le_bytes(buf) & 0x7F_FFFF)) & !(USER_PAGE_SIZE - 1)
    };

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

    // Refuse to exec a directory (EACCES).
    if exec_meta.kind() == InodeKind::Directory {
        return Err(ExecError::PermissionDenied);
    }

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

    // ===== Phase 2.5 — shebang (#!) + no-shebang script dispatch =====
    //
    // If the file starts with "#!" treat it as a script: parse the
    // interpreter path (and optional single argument) from the first
    // line, then re-invoke exec_script with the interpreter as the new
    // target.  Mirrors Linux binfmt_script.  One level of recursion is
    // sufficient (the interpreter itself must be a real ELF binary).
    //
    // If the file does NOT start with "#!" AND does NOT start with the
    // ELF magic `0x7f 'E' 'L' 'F'`, treat it as a `/bin/sh` script.
    // Linux's kernel doesn't do this — it returns -ENOEXEC and lets the
    // shell decide whether to interpret the file as a script. busybox
    // ash's ENOEXEC fallback only fires for files whose first character
    // looks "script-like"; oscomp's `run-static.sh` / `run-dynamic.sh`
    // begin with `./runtest.exe …` (no shebang) and ash gives up,
    // leaving libctest's 220 tests at 0/220 even though the scripts
    // are perfectly valid shell. Kernel-side fallback to `/bin/sh`
    // matches what every userspace shell *would* do if it dared, and
    // unblocks libctest end-to-end. (cf. STATUS.md 2026-05-18 — top
    // of the high-stakes table.)
    let is_shebang = header_bytes.starts_with(b"#!");
    let is_elf = header_bytes.len() >= 4 && &header_bytes[..4] == b"\x7fELF";
    if is_shebang {
        if let Some((interp, opt_arg)) = shebang_parse(&header_bytes) {
            EXEC_SHEBANG_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let (interp_path, new_argv) = shebang_exec_argv(interp, opt_arg, path, argv);
            let new_argv_refs: Vec<&[u8]> = new_argv.iter().map(|v| v.as_slice()).collect();
            return alloc::boxed::Box::pin(exec_script_inner::<P>(
                depth + 1,
                process,
                thread,
                &interp_path,
                &new_argv_refs,
                envp,
                cred,
            ))
            .await;
        }
    } else if !is_elf {
        // No `#!` and no ELF magic — synthesize `/bin/sh <path> [argv…]`.
        // Depth guard: don't recurse forever if `/bin/sh` itself is
        // somehow a non-ELF / non-shebang file (the recursion limit
        // upstream caps this; we lean on `depth + 1 < MAX_DEPTH`).
        const DEFAULT_SHELL: &[u8] = b"/bin/sh";
        let mut new_argv: Vec<Vec<u8>> = Vec::new();
        new_argv.push(DEFAULT_SHELL.to_vec());
        new_argv.push(path.to_vec());
        for &a in argv.iter().skip(1) {
            new_argv.push(a.to_vec());
        }
        let new_argv_refs: Vec<&[u8]> = new_argv.iter().map(|v| v.as_slice()).collect();
        return alloc::boxed::Box::pin(exec_script_inner::<P>(
            depth + 1,
            process,
            thread,
            DEFAULT_SHELL,
            &new_argv_refs,
            envp,
            cred,
        ))
        .await;
    }

    // ===== Phase 3 — parse + validate (pure CPU) =====================
    //
    // `txdoc:EXEC-8-3-PARSE-AND-VALIDATE`. Goblin-backed parser owns
    // every header and program-header check (class / data / version /
    // arch / type / phdr-table fits / congruence / overlap / W^X).
    // N69a accepts `PT_INTERP` on the main program and surfaces the
    // segment locator in `parsed.interp`; the orchestrator dereferences
    // the path bytes below and loads the interpreter alongside the main
    // image.  If parsing still rejects the image after the shebang /
    // non-ELF probes above, fall back to `/bin/sh` within the recursion
    // budget for OSComp-style script launchers.
    let mut parsed: ExecImagePlan = match parse_image_plan(&header_bytes) {
        Ok(plan) => plan,
        Err(parse_err) => {
            if !is_elf && depth < SHEBANG_MAX_DEPTH {
                let interp_path: Vec<u8> = b"/bin/sh".to_vec();
                let mut new_argv: Vec<Vec<u8>> = Vec::new();
                new_argv.push(interp_path.clone());
                new_argv.push(path.to_vec());
                for &a in argv.iter().skip(1) {
                    new_argv.push(a.to_vec());
                }
                let new_argv_refs: Vec<&[u8]> = new_argv.iter().map(|v| v.as_slice()).collect();
                return alloc::boxed::Box::pin(exec_script_inner::<P>(
                    depth + 1,
                    process,
                    thread,
                    &interp_path,
                    &new_argv_refs,
                    envp,
                    cred,
                ))
                .await;
            }
            return Err(ExecError::from_parse_error(parse_err));
        }
    };

    // ASLR: for ET_DYN images, shift the fixed load_bias by a
    // random offset.  ET_EXEC binaries (load_bias == 0) are not
    // randomised — they use absolute virtual addresses.
    let aslr_delta: i64 = if parsed.load_bias != 0 {
        let new_bias = randomize_et_dyn_base();
        let delta = (new_bias as i64) - (parsed.load_bias as i64);
        parsed.load_bias = new_bias;
        delta
    } else {
        0
    };
    // Apply delta to entry, at_phdr, and all LOAD segment vaddrs.
    parsed.entry = (parsed.entry as i64 + aslr_delta) as u64;
    parsed.at_phdr = (parsed.at_phdr as i64 + aslr_delta) as u64;
    for seg in &mut parsed.load_segments {
        seg.vaddr = (seg.vaddr as i64 + aslr_delta) as u64;
    }

    // ===== Phase 3a — open + parse interpreter (N69a) ================
    //
    // When the main image carries a `PT_INTERP`, read the interpreter
    // path bytes from the file, resolve it through the same VFS
    // walker, parse it as ET_DYN, and stage a vm-side image plan.
    // Nothing is mapped yet; the recipe rows land in Phase 4 right
    // after `build_aspace_from_image`.
    let interp_load = if let Some(interp_ref) = parsed.interp {
        Some(load_interp_image(&file_pc, &header_bytes, interp_ref, process, cred).await?)
    } else {
        None
    };

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
    //
    // nosuid mount check: if the file resides on a mount with the
    // `NOSUID` flag, skip S_ISUID/S_ISGID processing and keep the
    // caller's credentials.  `AT_SECURE` is not set.
    let mount_nosuid = openfile
        .rnode()
        .containing_mount_weak()
        .and_then(|w| {
            let guard = step_engine::guard();
            w.upgrade(&guard)
        })
        .is_some_and(|mp| mp.options.flags.contains(MountFlags::NOSUID));

    let at_secure = if mount_nosuid {
        false
    } else {
        let exec_outcome = step_apply_suid_for_exec(
            process,
            Uid(exec_meta.uid),
            Gid(exec_meta.gid),
            exec_meta.mode,
        );
        exec_outcome.is_some_and(|o| o.at_secure)
    };

    // ===== Phase 4 — build detached AddressSpace =====================
    //
    // `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`. Bridge the
    // parser's `LoadSegment` (which has no PageContainer — the
    // parser is purely byte-driven) to `vm::scripts::LoadSegment`
    // (which carries the `Cap<PageContainer>` the recipe row references
    // for demand-faulting bytes from the file). Every LOAD segment
    // shares the same backing `Cap` (they all view different ranges
    // of the same file).
    let interp_exec_stack = interp_load
        .as_ref()
        .is_some_and(|interp| interp.parsed.executable_stack);
    let stack_top = randomize_stack_top();
    let mut image_plan = build_vm_image_plan(&parsed, stack_top, &file_pc);
    if interp_exec_stack {
        image_plan.executable_stack = true;
    }
    // V1 (`build_aspace_from_image`) takes no `&Guard` — it acquires
    // its own per-call guards internally. Per
    // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` callers must NOT
    // hold a guard at the call site.
    let new_aspace = vm_scripts::build_aspace_from_image::<P>(&image_plan)
        .map_err(ExecError::from_build_aspace_error)?;

    // ===== Phase 4b — register interpreter LOAD segments (N69a) =====
    //
    // Stages the interp's recipe rows on the same detached aspace at
    // `INTERP_LOAD_BIAS_DEFAULT`. The main image owns the stack range;
    // the interpreter does not add one. Failures here drop the
    // detached aspace exactly like a Phase 4 failure would — no
    // observable side effect on the caller.
    if let Some(interp) = interp_load.as_ref() {
        vm_scripts::register_interp_image(&new_aspace, &interp.vm_plan, INTERP_LOAD_BIAS_DEFAULT)
            .map_err(ExecError::from_build_aspace_error)?;
    }

    // ===== Phase 5 — thread-group collapse (if multi-threaded) =======
    if let Some(payload) = process.payload_slot().lock().as_ref() {
        payload.install_exec_group_exit();
    }

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
    populate_partial_last_pages(&new_aspace, &image_plan.load_segments, 0, page_size).await?;
    if let Some(interp) = interp_load.as_ref() {
        populate_partial_last_pages(
            &new_aspace,
            &interp.vm_plan.load_segments,
            INTERP_LOAD_BIAS_DEFAULT,
            page_size,
        )
        .await?;
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
    tx_services::random::fill_bytes(&mut at_random_bytes);
    let arch = <P as tx_hal::AuxvIf>::arch_auxv_facts();

    let auxv_facts = AuxvFacts {
        at_phdr: parsed.at_phdr,
        at_phent: ELF64_PHENT,
        at_phnum: parsed.at_phnum,
        at_pagesz: USER_PAGE_SIZE,
        // N69a: when the main image carries a `PT_INTERP`, the
        // kernel stages the interpreter at `INTERP_LOAD_BIAS_DEFAULT`
        // and reports the bias here so musl's `__libc_start_main`
        // recognises this as an interpreter-loaded binary. Static
        // `ET_EXEC` (no PT_INTERP) keeps the historical `at_base = 0`.
        // `AT_ENTRY` always carries the **program's** entry through
        // to userspace; the initial PC below jumps into the
        // interpreter instead when one is present.
        at_base: if interp_load.is_some() {
            INTERP_LOAD_BIAS_DEFAULT
        } else {
            0
        },
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
        at_hwcap: arch.hwcap,
        at_hwcap2: arch.hwcap2,
        at_platform: None,
        platform_string: arch.platform.as_bytes(),
        at_clktck: super::stack::CLKTCK_VALUE,
        at_execfn: None,
        execfn_string: b"",
        at_flags: 0,
        at_sysinfo_ehdr: None,
    };
    let stack_image = build_initial_user_stack(stack_top, argv, envp, &auxv_facts);

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

    // N69a: when an interpreter is present the kernel jumps into it,
    // not into the program. The interpreter relocates itself, mmaps
    // any shared libs the program needs (musl's libc.so is both the
    // interpreter and the C runtime), then calls the program's
    // `_start` (whose address musl reads from auxv `AT_ENTRY`).
    let entry_pc = match interp_load.as_ref() {
        Some(interp) => (interp.parsed.entry + INTERP_LOAD_BIAS_DEFAULT) as usize,
        None => parsed.entry as usize,
    };
    let initial_sp = stack_image.initial_sp as usize;
    let user_ctx = make_initial_user_trap_context(P::ARCH, entry_pc, initial_sp);
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
    // Store executable reference, cmdline, AND comm for procfs and
    // observation (§EXEC_v1 §3.7). The PCB short name (`comm`) is the
    // basename of argv[0] truncated to 15 bytes + NUL — Linux
    // `TASK_COMM_LEN`. Populating it here closes the gap where the
    // process payload's `_comm` slot stayed `[0; 16]` for the entire
    // process lifetime, and lets OBS-V1 §15.7 ProcessLabel emits read
    // a meaningful name back from `process.comm()` (so Perfetto
    // shows e.g. `busybox` / `basic_exec` instead of `pid-<N>`).
    let mut committed_comm: Option<[u8; 16]> = None;
    if let Some(payload) = process.payload_slot().lock().as_ref() {
        if let Some(dentry) = openfile.opendir_dentry() {
            *payload._exe_file.lock() = Some(dentry.clone());
        }
        let argv0 = argv.first().copied().unwrap_or(b"");
        // basename(argv[0]) — strip everything up to the last `/`.
        let comm_src = match argv0.iter().rposition(|&b| b == b'/') {
            Some(i) => &argv0[i + 1..],
            None => argv0,
        };
        let mut comm_buf = [0u8; 16];
        let n = core::cmp::min(comm_src.len(), 15);
        comm_buf[..n].copy_from_slice(&comm_src[..n]);
        *payload._comm.lock() = comm_buf;
        let cmdline_bytes = argv0.to_vec();
        *payload._cmdline.lock() = Some(cmdline_bytes);
        committed_comm = Some(comm_buf);
    }
    // OBS-V1 §15.7 + §15.8: re-emit the PCB identity bundle once
    // the payload-slot lock has been dropped — `pgrp_cap()` takes
    // its own spin lock and the observation emit path may itself
    // touch unrelated process state, so doing it inside the
    // payload-lock scope risks lock-order surprises (we previously
    // hung under busybox here). Splitting the emit out keeps each
    // lock acquisition flat and short.
    if let Some(comm_buf) = committed_comm {
        emit_process_label_for(process.pid.0, &comm_buf);
        let pgrp = process.pgrp_cap();
        let sid = pgrp.session_cap().sid.0;
        emit_process_group_for(process.pid.0, pgrp.pgid.0, sid);
    }
    // vfork completion: if the parent is waiting on CLONE_VFORK,
    // unblock it now that exec has completed.
    process.fire_exit_source(Mask::from_bits(1));

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

/// Eager-populate every BSS-tail-extending LOAD segment's
/// partial-last-page from the file. Shared between the main image and
/// the N69a interpreter image.
///
/// `load_bias` is the offset already applied (or to be applied) when
/// the segment's recipe row was registered — zero for the main image,
/// `INTERP_LOAD_BIAS_DEFAULT` for the interpreter. `page_size` is the
/// platform's user page size (4096 on RV64).
///
/// Per the ELF spec, bytes in the LAST file-backed page of a LOAD
/// segment beyond `vaddr + filesz` must read as zero. The recipe
/// registration in `register_load_segment` rounds the file-backed
/// range DOWN to a page boundary when there's a BSS extension; this
/// loop eagerly populates that page's file-content prefix from the
/// `PageContainer`, leaving the rest of the page zero (fresh anon
/// allocation). Without this fix, RX segments would expose adjacent
/// file bytes past `filesz` (busybox.musl previously crashed
/// dereferencing `.got\0ata` string fragments via gp-relative loads).
async fn populate_partial_last_pages(
    aspace: &Cap<tx_subsystems::vm::AddressSpace>,
    segments: &[VmLoadSegment],
    load_bias: u64,
    page_size: u64,
) -> Result<(), ExecError> {
    for segment in segments {
        if segment.filesz == 0 || segment.memsz <= segment.filesz {
            continue;
        }
        let biased_vaddr = segment
            .vaddr
            .checked_add(load_bias)
            .ok_or(ExecError::NotExecutable)?;
        let file_end = biased_vaddr
            .checked_add(segment.filesz)
            .ok_or(ExecError::NotExecutable)?;
        let partial_in_page = file_end & (page_size - 1);
        if partial_in_page == 0 {
            continue;
        }
        let partial_start = (file_end - partial_in_page).max(biased_vaddr);
        // File offset of `partial_start`. Segment's `file_offset`
        // corresponds to its (relative) `vaddr`; offset by
        // `partial_start - biased_vaddr` to reach the bytes we need to
        // seed. If the segment starts mid-page, the page floor can
        // precede `vaddr`, so `partial_start` is clamped to the segment
        // start.
        let file_off = segment
            .file_offset
            .checked_add(partial_start - biased_vaddr)
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
        match vm_scripts::populate_detached_user_range(aspace, partial_start, &buf).await {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return Err(ExecError::Busy);
            }
            StepOutcome::Err(err) => {
                return Err(ExecError::from_populate_errno(err.into()));
            }
        }
    }
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
fn make_initial_user_trap_context(arch: tx_hal::Arch, pc: usize, sp: usize) -> UserTrapContext {
    let mut regs = [0usize; 32];
    // Arch-specific SP register (x2 on RV64, x3 on LA64).
    regs[initial_user_sp_reg_for_arch(arch)] = sp;
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

const fn translate_flags(parsed: ParsedSegmentFlags) -> VmSegmentFlags {
    VmSegmentFlags {
        read: parsed.readable,
        write: parsed.writable,
        execute: parsed.executable,
    }
}

/// Staged interpreter image (N69a). Built before Phase 4 so the main
/// `build_aspace_from_image` call still owns aspace allocation while
/// the interpreter contributes only recipe rows.
struct InterpLoad {
    /// Parser output — used for the initial PC (`parsed.entry +
    /// INTERP_LOAD_BIAS_DEFAULT`).
    parsed: ExecImagePlan,
    /// vm-scripts view, with the interpreter file `PageContainer`
    /// stitched into every LOAD segment's `backing` — consumed by
    /// `register_interp_image` and `populate_partial_last_pages`.
    vm_plan: VmImagePlan,
}

/// Resolve a `PT_INTERP` reference into an `InterpLoad`.
///
/// `main_header_bytes` is the kernel's initial 4 KiB window over the
/// main image; we slice the interp path from it when the PT_INTERP
/// segment is entirely covered (the canonical case for musl-linked
/// binaries, where the segment sits in the first few hundred bytes of
/// the file). Otherwise the function reads the path bytes through the
/// main file's `PageContainer`.
///
/// The interpreter is opened via the same VFS walker the main file
/// went through. Linux does not require the interpreter to carry an
/// X bit (`fs/binfmt_elf.c::load_elf_binary` opens it with
/// `MAY_READ | MAY_EXEC` against `init_cred`, ignoring the file's
/// X bit for ELF-spec compatibility — old toolchains shipped `libc.so`
/// without X). We follow the same policy: read-bit only.
async fn load_interp_image(
    main_file_pc: &Cap<PageContainer>,
    main_header_bytes: &[u8],
    interp_ref: super::loader::InterpRef,
    process: &Cap<ProcessIdentity>,
    cred: &Credential,
) -> Result<InterpLoad, ExecError> {
    // --- 1. extract the interpreter path string from the main file -------
    //
    // Hard cap on path length: 4096 bytes is Linux's `PATH_MAX`. The
    // PT_INTERP segment carries `path\0`; the trailing NUL counts toward
    // `filesz` and is stripped here.
    if interp_ref.filesz == 0 || interp_ref.filesz > 4096 {
        return Err(ExecError::NotExecutable);
    }
    let need = interp_ref.filesz as usize;
    let start = interp_ref.file_offset as usize;
    let end = start.checked_add(need).ok_or(ExecError::NotExecutable)?;

    let mut path_buf: Vec<u8>;
    let path_bytes_full: &[u8] = if end <= main_header_bytes.len() {
        &main_header_bytes[start..end]
    } else {
        path_buf = alloc::vec![0u8; need];
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let outcome = read_exact_at(main_file_pc, interp_ref.file_offset, &mut path_buf, &guard);
        let result = match outcome {
            V3::Done(()) => Ok(()),
            V3::Continue { .. } | V3::Yield { .. } => Err(ExecError::Busy),
            V3::Err(_) => Err(ExecError::NotExecutable),
        };
        drop(guard);
        result?;
        &path_buf[..]
    };

    // Strip trailing NULs (the toolchain emits exactly one; we tolerate
    // alignment padding too).
    let trimmed_end = path_bytes_full
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let interp_path = &path_bytes_full[..trimmed_end];
    if interp_path.is_empty() {
        return Err(ExecError::NotExecutable);
    }

    // --- 2. resolve + open the interpreter through the VFS walker -------
    //
    // Linux requires read access only; the X bit on `libc.so` is not
    // checked because the kernel maps it via `mmap` semantics rather
    // than `execve`. Errors here translate the walker's errno through
    // the same `from_walker_errno` mapper the main file used.
    let interp_open = {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let rooted_at = process.cwd().ok_or(ExecError::PathNotFound)?;
        let outcome = step_open(
            rooted_at,
            interp_path,
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
            V3::Err(err) => Err(ExecError::from_walker_errno(Errno::from(err))),
        };
        drop(guard);
        result?
    };

    let interp_file_pc = match interp_open.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return Err(ExecError::NotExecutable),
    };
    let interp_size = interp_file_pc.size_bytes();

    // --- 3. read interpreter header bytes and parse as ET_DYN ----------
    let interp_read_len = core::cmp::min(interp_size as usize, INITIAL_PARSE_READ);
    if interp_read_len < 64 {
        return Err(ExecError::NotExecutable);
    }
    let mut interp_header_bytes: Vec<u8> = alloc::vec![0u8; interp_read_len];
    {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let outcome = read_exact_at(&interp_file_pc, 0, &mut interp_header_bytes, &guard);
        let result = match outcome {
            V3::Done(()) => Ok(()),
            V3::Continue { .. } | V3::Yield { .. } => Err(ExecError::Busy),
            V3::Err(err) => Err(ExecError::from_read_errno(err.into())),
        };
        drop(guard);
        result?;
    }
    let interp_parsed: ExecImagePlan =
        parse_interp_plan(&interp_header_bytes).map_err(ExecError::from_parse_error)?;

    // --- 4. bridge to the vm-side image plan --------------------------
    let vm_plan = build_vm_image_plan(&interp_parsed, USER_STACK_TOP_DEFAULT, &interp_file_pc);

    Ok(InterpLoad {
        parsed: interp_parsed,
        vm_plan,
    })
}

/// Bridge `parse_image_plan`'s view (no PageContainer) to the
/// vm-scripts view (LOAD segments carry the file's `Cap<PageContainer>`).
fn build_vm_image_plan(
    parsed: &ExecImagePlan,
    stack_top: u64,
    file_pc: &Cap<PageContainer>,
) -> VmImagePlan {
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
        stack_top,
        load_segments,
        bss_extension,
        executable_stack: parsed.executable_stack,
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
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
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

/// Build argv for a shebang re-exec.
///
/// OSComp Lua uses helper scripts with `#!/bin/busybox sh`. Txv2
/// publishes `/bin/sh` consistently across boot modes, while
/// `/bin/busybox` is not guaranteed to exist. When normalising that
/// exact shebang to `/bin/sh`, the original busybox applet selector
/// (`sh`) must be consumed; otherwise busybox receives
/// `/bin/sh sh script ...` and tries to open a script literally named
/// `sh`.
fn shebang_exec_argv(
    interp: &[u8],
    opt_arg: Option<&[u8]>,
    script_path: &[u8],
    original_argv: &[&[u8]],
) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut interp_path = interp.to_vec();
    let mut opt_arg = opt_arg;
    if interp == b"/bin/busybox" {
        interp_path = b"/bin/sh".to_vec();
        if matches!(opt_arg, Some(b"sh" | b"ash")) {
            opt_arg = None;
        }
    }

    // Linux binfmt_script shape: [interp, opt_arg?, script_path,
    // original argv[1..]...].
    let mut new_argv: Vec<Vec<u8>> = Vec::new();
    new_argv.push(interp_path.clone());
    if let Some(arg) = opt_arg {
        new_argv.push(arg.to_vec());
    }
    new_argv.push(script_path.to_vec());
    for &a in original_argv.iter().skip(1) {
        new_argv.push(a.to_vec());
    }
    (interp_path, new_argv)
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
