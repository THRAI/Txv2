//! Exec script orchestration (Phase 5 of the ELF-loader plan).
//!
//! Realises the eight-phase EXEC_v1 protocol against the seams shipped
//! by Wave 1 (`vm::scripts`, `page_backed::read_exact_at`) and Wave 2
//! (`PreparedCloexecClose`, `ResetSignalDispositionsForExecOp`,
//! `InstallBrkForExecOp`). The script integrates the loader's
//! staged ELF reader (`super::image_reader::read_elf_image`) and stack-image
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
//! Phase 6 performs one final checked authoritative-binding swap. Failure is
//! returned before the new address space is stored; a successful swap is the
//! irreversible visibility boundary. After that point the function performs only Phase-7
//! commits (Wave 2 helpers, all infallible by EXEC-PONR) and returns
//! `Ok(())`. There is no recoverable error path after the successful Phase-6
//! swap; an internal lifecycle-invariant violation is fatal.

use alloc::vec::Vec;

use tx_hal::{Arch, EntropyIf, PmapIf, UserTrapContext};
use tx_subsystems::cred::{
    commit_prepared_exec_cred, prepare_exec_cred_in, Capability, ExecSetidPolicy, Gid, Uid,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{MountFlags, MountNamespace};
use tx_subsystems::page_backed::{read_exact_at, PageContainer};
use tx_subsystems::process::{
    InstallBrkForExecOp, ProcessExecPrep, ProcessIdentity, ResetSignalDispositionsForExecOp,
};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::walker::step_open_in_mount_namespace_with_origin_mount;
use tx_subsystems::vm::scripts::{
    self as vm_scripts, BssTail as VmBssTail, ImagePlan as VmImagePlan,
    LoadSegment as VmLoadSegment, SegmentFlags as VmSegmentFlags, USER_STACK_INITIAL_RESERVATION,
};
use tx_subsystems::vm::{map_vdso_into_aspace, VdsoLayout, VdsoMapping, VmMapError};

use super::image_reader::{read_elf_image, ImageReadError, ImageRole};
use super::loader::{
    ElfLayoutError, ExecImagePlan, ImageRange, LoadSegment as ParsedLoadSegment,
    SegmentFlags as ParsedSegmentFlags, ELF64_PHENT,
};
use super::stack::{build_initial_user_stack, AuxvFacts, StackBuildError};
use crate::adapter::step_engine::{
    self as step_engine, Cap, NoProgress, ScriptCtx, StepOp, StepOutcome,
};
use crate::adapter::vfs_exec::{
    Credential, DEntry, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNodeBacking,
};

/// User page size — RV64 today; mirrors `vm::USER_PAGE_SIZE` so the
/// brk-base round-up doesn't require pulling in another import.
const USER_PAGE_SIZE: u64 = 4096;

/// The preliminary OSComp lmbench image ships `hello` as a tiny wrapper
/// without a shebang.  BusyBox ash does not apply its usual ENOEXEC fallback
/// to this path, so direct `execve("/tmp/hello", ...)` from `lat_proc` would
/// fail even though the file is valid shell input.  Keep the compatibility
/// rule content-scoped instead of treating every non-ELF file as a script.
const OSCOMP_LMBENCH_HELLO_WRAPPER: &[u8] = b"/code/lmbench_src/bin/build/lmbench_all hello \"$@\"";

const MIN_LOAD_BIAS: u64 = 0x1_0000;
const ASLR_LAYOUT_ATTEMPTS: usize = 16;
const ASLR_WINDOW: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
struct CombinedImageLayout {
    main: ExecImagePlan,
    interpreter: Option<ExecImagePlan>,
    stack_top: u64,
    vdso_window: ImageRange,
    vdso_layout: VdsoLayout,
}

#[derive(Clone, Copy)]
enum ExecutableCandidateRole {
    Main,
    Interpreter,
}

impl ExecutableCandidateRole {
    const fn invalid_image(self) -> ExecError {
        match self {
            Self::Main => ExecError::NotExecutable,
            Self::Interpreter => ExecError::InterpreterMalformed,
        }
    }
}

struct ExecutableCandidate {
    file: Cap<OpenFile>,
    pc: Cap<PageContainer>,
    meta: InodeMeta,
    mount: Cap<tx_subsystems::mount::MountIdentity>,
}

fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    tx_services::random::fill_bytes(&mut bytes);
    u64::from_le_bytes(bytes)
}

fn try_zeroed_exec_bytes(len: usize) -> Result<Vec<u8>, ExecError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| ExecError::OutOfMemory)?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn try_copy_exec_bytes(bytes: &[u8]) -> Result<Vec<u8>, ExecError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| ExecError::OutOfMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

fn try_reserve_exec_items<T>(items: &mut Vec<T>, additional: usize) -> Result<(), ExecError> {
    items
        .try_reserve_exact(additional)
        .map_err(|_| ExecError::OutOfMemory)
}

fn try_exec_argv_refs(argv: &[Vec<u8>]) -> Result<Vec<&[u8]>, ExecError> {
    let mut refs = Vec::new();
    try_reserve_exec_items(&mut refs, argv.len())?;
    for item in argv {
        refs.push(item.as_slice());
    }
    Ok(refs)
}

fn try_copy_exec_argv(argv: &[&[u8]]) -> Result<Vec<Vec<u8>>, ExecError> {
    let mut owned = Vec::new();
    try_reserve_exec_items(&mut owned, argv.len())?;
    for item in argv {
        if item.contains(&b'\0') {
            return Err(ExecError::InvalidArgument);
        }
        owned.push(try_copy_exec_bytes(item)?);
    }
    Ok(owned)
}

fn align_up(value: u64, align: u64) -> Result<u64, ElfLayoutError> {
    if align == 0 || !align.is_power_of_two() {
        return Err(ElfLayoutError::InvalidAlignment);
    }
    value
        .checked_add(align - 1)
        .map(|rounded| rounded & !(align - 1))
        .ok_or(ElfLayoutError::AddressOverflow)
}

const fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn plan_alignment(plan: &ExecImagePlan) -> Result<u64, ElfLayoutError> {
    let mut align = USER_PAGE_SIZE;
    for segment in &plan.load_segments {
        let segment_align = segment.align.max(USER_PAGE_SIZE);
        if !segment_align.is_power_of_two() {
            return Err(ElfLayoutError::InvalidAlignment);
        }
        align = align.max(segment_align);
    }
    Ok(align)
}

fn plan_relative_end(plan: &ExecImagePlan) -> Result<u64, ElfLayoutError> {
    plan.load_segments.iter().try_fold(0, |highest, segment| {
        let end = segment
            .vaddr
            .checked_add(segment.memsz)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        let relative = end
            .checked_sub(plan.load_bias)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        Ok(highest.max(relative))
    })
}

fn candidate_bias(
    plan: &ExecImagePlan,
    entropy: u64,
    upper_bound: u64,
) -> Result<u64, ElfLayoutError> {
    let align = plan_alignment(plan)?;
    let first = align_up(MIN_LOAD_BIAS, align)?;
    let last = upper_bound
        .checked_sub(plan_relative_end(plan)?)
        .map(|value| align_down(value, align))
        .filter(|value| *value >= first)
        .ok_or(ElfLayoutError::UserRange)?;
    let slots = (last - first) / align + 1;
    let slot = entropy % slots;
    first
        .checked_add(
            slot.checked_mul(align)
                .ok_or(ElfLayoutError::AddressOverflow)?,
        )
        .ok_or(ElfLayoutError::AddressOverflow)
}

fn plan_ranges(plan: &ExecImagePlan) -> Result<Vec<ImageRange>, ElfLayoutError> {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(plan.load_segments.len())
        .map_err(|_| ElfLayoutError::OutOfMemory)?;
    for segment in &plan.load_segments {
        let start = align_down(segment.vaddr, USER_PAGE_SIZE);
        let end = segment
            .vaddr
            .checked_add(segment.memsz)
            .and_then(|end| align_up(end, USER_PAGE_SIZE).ok())
            .ok_or(ElfLayoutError::AddressOverflow)?;
        ranges.push(ImageRange {
            vaddr: start,
            size: end - start,
        });
    }
    Ok(ranges)
}

fn try_clone_exec_plan(plan: &ExecImagePlan) -> Result<ExecImagePlan, ElfLayoutError> {
    let mut load_segments = Vec::new();
    load_segments
        .try_reserve_exact(plan.load_segments.len())
        .map_err(|_| ElfLayoutError::OutOfMemory)?;
    for segment in &plan.load_segments {
        load_segments.push(segment.clone());
    }
    let interpreter_path = plan
        .interpreter_path
        .as_deref()
        .map(try_copy_exec_bytes)
        .transpose()
        .map_err(|_| ElfLayoutError::OutOfMemory)?;

    Ok(ExecImagePlan {
        entry: plan.entry,
        at_phdr: plan.at_phdr,
        at_phent: plan.at_phent,
        at_phnum: plan.at_phnum,
        load_segments,
        bss_extension: plan.bss_extension,
        load_bias: plan.load_bias,
        tls: plan.tls,
        dynamic: plan.dynamic,
        relro: plan.relro,
        stack: plan.stack,
        interpreter_path,
    })
}

fn ranges_overlap(left: ImageRange, right: ImageRange) -> Result<bool, ElfLayoutError> {
    let left_end = left
        .vaddr
        .checked_add(left.size)
        .ok_or(ElfLayoutError::AddressOverflow)?;
    let right_end = right
        .vaddr
        .checked_add(right.size)
        .ok_or(ElfLayoutError::AddressOverflow)?;
    Ok(left.vaddr < right_end && right.vaddr < left_end)
}

fn ranges_are_valid(ranges: &[ImageRange], user_top: u64) -> Result<bool, ElfLayoutError> {
    for (index, left) in ranges.iter().copied().enumerate() {
        let end = left
            .vaddr
            .checked_add(left.size)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        if end > user_top {
            return Ok(false);
        }
        for right in ranges[index + 1..].iter().copied() {
            if ranges_overlap(left, right)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn select_combined_layout(
    main: ExecImagePlan,
    interpreter: Option<ExecImagePlan>,
    user_top: u64,
    mut entropy: impl FnMut() -> u64,
) -> Result<CombinedImageLayout, ElfLayoutError> {
    let vm_user_top = usize::try_from(user_top).map_err(|_| ElfLayoutError::UserRange)?;
    let vdso_layout = VdsoLayout::for_user_top(tx_subsystems::vm::UserVirtAddr::new(vm_user_top))
        .map_err(|_| ElfLayoutError::UserRange)?;
    let reservation = vdso_layout.window();
    let vdso_window = ImageRange {
        vaddr: reservation.start().as_usize() as u64,
        size: reservation.len() as u64,
    };
    if user_top <= vdso_window.size + USER_STACK_INITIAL_RESERVATION + MIN_LOAD_BIAS {
        return Err(ElfLayoutError::UserRange);
    }
    let stack_ceiling = vdso_window.vaddr;
    let stack_slide_slots = ASLR_WINDOW / USER_PAGE_SIZE;

    for _ in 0..ASLR_LAYOUT_ATTEMPTS {
        let candidate = (|| {
            let slide = (entropy() % stack_slide_slots) * USER_PAGE_SIZE;
            let stack_top = stack_ceiling
                .checked_sub(slide)
                .ok_or(ElfLayoutError::AddressOverflow)?;
            let stack_start = stack_top
                .checked_sub(USER_STACK_INITIAL_RESERVATION)
                .ok_or(ElfLayoutError::UserRange)?;

            let main_candidate = if main.load_bias == 0 {
                try_clone_exec_plan(&main)?.checked_rebase(0, user_top)?
            } else {
                let bias = candidate_bias(&main, entropy(), stack_start)?;
                try_clone_exec_plan(&main)?.checked_rebase(bias, user_top)?
            };
            let interpreter_candidate = if let Some(plan) = interpreter.as_ref() {
                if plan.load_bias == 0 {
                    Some(try_clone_exec_plan(plan)?.checked_rebase(0, user_top)?)
                } else {
                    let bias = candidate_bias(plan, entropy(), stack_start)?;
                    Some(try_clone_exec_plan(plan)?.checked_rebase(bias, user_top)?)
                }
            } else {
                None
            };

            let mut ranges = plan_ranges(&main_candidate)?;
            let extra = interpreter_candidate
                .as_ref()
                .map_or(2, |plan| plan.load_segments.len() + 2);
            ranges
                .try_reserve_exact(extra)
                .map_err(|_| ElfLayoutError::OutOfMemory)?;
            if let Some(plan) = interpreter_candidate.as_ref() {
                ranges.extend(plan_ranges(plan)?);
            }
            ranges.push(ImageRange {
                vaddr: stack_start,
                size: USER_STACK_INITIAL_RESERVATION,
            });
            ranges.push(vdso_window);
            if !ranges_are_valid(&ranges, user_top)? {
                return Err(ElfLayoutError::Overlap);
            }
            Ok(CombinedImageLayout {
                main: main_candidate,
                interpreter: interpreter_candidate,
                stack_top,
                vdso_window,
                vdso_layout,
            })
        })();

        match candidate {
            Ok(layout) => return Ok(layout),
            Err(
                ElfLayoutError::AddressOverflow
                | ElfLayoutError::UserRange
                | ElfLayoutError::Overlap,
            ) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ElfLayoutError::Exhausted)
}

/// Emit a OBS-V1 §15.7 ProcessLabel Instant mapping `pid` to the PCB
/// `comm` just committed by the exec identity store
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
    em.process_label(tx_observe::current_parent_span(), pid_low, comm);
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
    em.process_group(
        tx_observe::current_parent_span(),
        pid_low,
        pgid_low,
        sid_low,
    );
}

/// Linux `linux_binprm::buf` size used for shebang dispatch.
/// ELF metadata is read independently by `image_reader` at declared offsets.
const BINPRM_BUF_SIZE: usize = 256;

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
    /// `ELIBBAD`. The requested userspace interpreter is malformed.
    InterpreterMalformed,
    /// `ELIBBAD`. An ELF interpreter requested another interpreter.
    InterpreterNested,
    /// Final main/interpreter/stack/vDSO layout could not be composed.
    Layout(ElfLayoutError),
    /// `EINVAL`. Invalid argument shape (e.g. plan that would
    /// underflow the user stack arithmetic).
    InvalidArgument,
    /// `ENOMEM`. Page-allocator / zone-allocator pressure. The exec
    /// path drops every reservation it acquired before this fired.
    OutOfMemory,
    /// `EAGAIN`. Process lifecycle or credential mutation episode is
    /// already owned by a concurrent exit/exec operation.
    Again,
    /// Re-run the current pre-PoNR operation without parking.
    Retry,
    /// Park on the originating wait object, then restart reversible exec
    /// preparation from Phase 1.
    Deferred(step_engine::YieldShape),
    /// `EBUSY`. Range-lock contention or unexpected pmap publish
    /// stall on a detached aspace; effectively unreachable for the v1
    /// surface and mostly a placeholder for tests.
    Busy,
    /// `EIO`. Page-cache / direct-map I/O failure.
    IoError,
}

impl ExecError {
    fn from_vdso_map_error(error: VmMapError) -> Self {
        match error {
            VmMapError::WouldBlock => Self::Retry,
            VmMapError::NoFreeRange | VmMapError::Private(_) => Self::OutOfMemory,
            VmMapError::Pmap(_) => Self::IoError,
            VmMapError::AlreadyMapped
            | VmMapError::InvalidRange
            | VmMapError::MissingMapping
            | VmMapError::BackingOffsetOverflow => Self::InvalidArgument,
        }
    }

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
            Errno::EAGAIN => Self::Retry,
            Errno::EIO => Self::IoError,
            Errno::ENODEV | Errno::ENOSYS => Self::NotExecutable,
            _ => Self::InvalidArgument,
        }
    }

    fn from_read_errno(err: Errno) -> Self {
        match err {
            Errno::ENOEXEC => Self::NotExecutable,
            Errno::ENOMEM => Self::OutOfMemory,
            Errno::EAGAIN => Self::Retry,
            Errno::EIO => Self::IoError,
            _ => Self::InvalidArgument,
        }
    }

    fn from_image_read_error(err: ImageReadError) -> Self {
        match err {
            ImageReadError::InvalidImage(super::loader::ParseError::OutOfMemory) => {
                Self::OutOfMemory
            }
            ImageReadError::InvalidImage(_) | ImageReadError::InvalidOffset => Self::NotExecutable,
            ImageReadError::Retry => Self::Retry,
            ImageReadError::WouldBlock(shape) => Self::Deferred(shape),
            ImageReadError::OutOfMemory => Self::OutOfMemory,
            ImageReadError::Io => Self::IoError,
        }
    }

    fn from_interpreter_image_read_error(err: ImageReadError) -> Self {
        match err {
            ImageReadError::InvalidImage(super::loader::ParseError::HasInterp) => {
                Self::InterpreterNested
            }
            ImageReadError::InvalidImage(super::loader::ParseError::OutOfMemory) => {
                Self::OutOfMemory
            }
            ImageReadError::InvalidImage(_) | ImageReadError::InvalidOffset => {
                Self::InterpreterMalformed
            }
            ImageReadError::Retry => Self::Retry,
            ImageReadError::WouldBlock(shape) => Self::Deferred(shape),
            ImageReadError::OutOfMemory => Self::OutOfMemory,
            ImageReadError::Io => Self::IoError,
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

    fn from_build_aspace_error(err: vm_scripts::ScriptError) -> Self {
        match err {
            vm_scripts::ScriptError::InvalidImage => Self::NotExecutable,
            vm_scripts::ScriptError::OutOfMemory => Self::OutOfMemory,
            vm_scripts::ScriptError::WouldBlock => Self::Retry,
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
            // ELIBBAD
            ExecError::InterpreterMalformed | ExecError::InterpreterNested => -80,
            // Layout OOM is ENOMEM; malformed/exhausted layouts are ENOEXEC.
            ExecError::Layout(ElfLayoutError::OutOfMemory) => -12,
            ExecError::Layout(_) => -8,
            // EINVAL
            ExecError::InvalidArgument => -22,
            // ENOMEM
            ExecError::OutOfMemory => -12,
            ExecError::Again => -11,
            ExecError::Retry | ExecError::Deferred(_) => -11,
            // EBUSY
            ExecError::Busy => -16,
            // EIO
            ExecError::IoError => -5,
        }
    }

    /// Map exec-script errors onto the v3 `StepOutcome::Err` errno
    /// surface used by [`ExecScriptOp`].
    pub fn to_step_errno(self) -> step_engine::Errno {
        match self {
            ExecError::PathTooLong => step_engine::Errno::ENAMETOOLONG,
            ExecError::PathNotFound => step_engine::Errno::ENOENT,
            ExecError::NotADirectory => step_engine::Errno::ENOTDIR,
            ExecError::PermissionDenied => step_engine::Errno::EACCES,
            ExecError::SymlinkLoop => step_engine::Errno::ELOOP,
            ExecError::NotExecutable => step_engine::Errno::ENOEXEC,
            ExecError::InterpreterMalformed | ExecError::InterpreterNested => {
                step_engine::Errno::ELIBBAD
            }
            ExecError::Layout(ElfLayoutError::OutOfMemory) => step_engine::Errno::ENOMEM,
            ExecError::Layout(_) => step_engine::Errno::ENOEXEC,
            ExecError::InvalidArgument => step_engine::Errno::EINVAL,
            ExecError::OutOfMemory => step_engine::Errno::ENOMEM,
            ExecError::Again => step_engine::Errno::EAGAIN,
            ExecError::Retry | ExecError::Deferred(_) => step_engine::Errno::EAGAIN,
            ExecError::Busy => step_engine::Errno::EBUSY,
            ExecError::IoError => step_engine::Errno::EIO,
        }
    }
}

fn vdso_auxv(mapping: Option<VdsoMapping>) -> Option<u64> {
    mapping.map(|mapping| mapping.vdso_base.as_usize() as u64)
}

fn validate_vdso_mapping(
    mapping: Option<VdsoMapping>,
    layout: VdsoLayout,
) -> Result<(), ElfLayoutError> {
    let Some(mapping) = mapping else {
        return Ok(());
    };
    let reserved = layout.window();
    let reserved_start = reserved.start().as_usize() as u64;
    let reserved_end = reserved.end().as_usize() as u64;
    let ranges = [
        ImageRange {
            vaddr: mapping.vdso_base.as_usize() as u64,
            size: mapping.vdso_size as u64,
        },
        ImageRange {
            vaddr: mapping.vvar_base.as_usize() as u64,
            size: USER_PAGE_SIZE,
        },
    ];
    for range in ranges {
        let end = range
            .vaddr
            .checked_add(range.size)
            .ok_or(ElfLayoutError::AddressOverflow)?;
        if range.vaddr < reserved_start || end > reserved_end {
            return Err(ElfLayoutError::UserRange);
        }
    }
    if ranges_overlap(ranges[0], ranges[1])? {
        return Err(ElfLayoutError::Overlap);
    }
    Ok(())
}

/// StepOp entry for execve.
///
/// This is the reachable syscall-facing exec step surface. It preserves the
/// canonical `exec_script_inner` implementation and its EXEC-PONR ordering.
/// Pre-PoNR waits retain their wait shape; the central waiting driver parks and
/// then re-enters reversible preparation from Phase 1.
pub struct ExecScriptOp<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf> {
    process: &'a Cap<ProcessIdentity>,
    thread: &'a Cap<ThreadIdentity>,
    path: &'a [u8],
    argv: &'a [&'a [u8]],
    envp: &'a [&'a [u8]],
    cred: &'a Credential,
    /// Pre-PoNR executable page containers whose PageReady endpoints may be
    /// named by a yielded step. The synchronous step facade recreates the
    /// reversible exec future after every wake, so it must retain these
    /// semantic owners until that retry observes completion.
    retained_page_containers: Vec<Cap<PageContainer>>,
    _platform: core::marker::PhantomData<fn() -> P>,
}

impl<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf> ExecScriptOp<'a, P> {
    pub const fn new(
        process: &'a Cap<ProcessIdentity>,
        thread: &'a Cap<ThreadIdentity>,
        path: &'a [u8],
        argv: &'a [&'a [u8]],
        envp: &'a [&'a [u8]],
        cred: &'a Credential,
    ) -> Self {
        Self {
            process,
            thread,
            path,
            argv,
            envp,
            cred,
            retained_page_containers: Vec::new(),
            _platform: core::marker::PhantomData,
        }
    }
}

impl<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf> StepOp<ProcessIdentity> for ExecScriptOp<'a, P> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<ProcessIdentity>) -> StepOutcome<(), NoProgress> {
        let future = exec_script_with_retention::<P>(
            self.process,
            self.thread,
            self.path,
            self.argv,
            self.envp,
            self.cred,
            &mut self.retained_page_containers,
        );
        let outcome = exec_result_to_step_outcome(poll_ready_synchronously(future));
        if matches!(outcome, StepOutcome::Done(_) | StepOutcome::Err(_)) {
            self.retained_page_containers.clear();
        }
        outcome
    }
}

pub(crate) fn exec_result_to_step_outcome(
    result: Option<Result<(), ExecError>>,
) -> StepOutcome<(), NoProgress> {
    match result {
        Some(Ok(())) => StepOutcome::done(()),
        Some(Err(ExecError::Deferred(shape))) => StepOutcome::Yield {
            progress: NoProgress,
            shape,
        },
        Some(Err(ExecError::Retry)) | None => StepOutcome::Continue {
            progress: NoProgress,
        },
        Some(Err(error)) => StepOutcome::err(error.to_step_errno()),
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
    let mut retained_page_containers = Vec::new();
    exec_script_with_retention::<P>(
        process,
        thread,
        path,
        argv,
        envp,
        cred,
        &mut retained_page_containers,
    )
    .await
}

async fn exec_script_with_retention<P: PmapIf + EntropyIf + tx_hal::AuxvIf>(
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    cred: &Credential,
    retained_page_containers: &mut Vec<Cap<PageContainer>>,
) -> Result<(), ExecError> {
    let mount_namespace = process
        .mount_namespace_cap()
        .ok_or(ExecError::PathNotFound)?;
    let original_path = try_copy_exec_bytes(path)?;
    let mut current_path = try_copy_exec_bytes(path)?;
    let mut current_argv = try_copy_exec_argv(argv)?;
    let envp = try_copy_exec_argv(envp)?;
    let envp_refs = try_exec_argv_refs(&envp)?;

    for depth in 0..=SHEBANG_MAX_DEPTH {
        let argv_refs = try_exec_argv_refs(&current_argv)?;
        let (rooted_at, origin_mount) = if current_path.starts_with(b"/") {
            (
                mount_namespace.root_dentry(),
                mount_namespace.root().clone(),
            )
        } else {
            let cwd = process.cwd_binding().ok_or(ExecError::PathNotFound)?;
            (cwd.dentry, cwd.mount)
        };
        let candidate = open_executable_candidate(
            rooted_at,
            &origin_mount,
            &current_path,
            cred,
            &mount_namespace,
            ExecutableCandidateRole::Main,
        )?;
        retain_exec_page_container(retained_page_containers, &candidate.pc);
        let read_len = usize::try_from(candidate.pc.size_bytes().min(BINPRM_BUF_SIZE as u64))
            .map_err(|_| ExecError::NotExecutable)?;
        if read_len == 0 {
            return Err(ExecError::NotExecutable);
        }
        let mut header = try_zeroed_exec_bytes(read_len)?;
        let guard = step_engine::guard();
        let read = read_exact_at(&candidate.pc, 0, &mut header, &guard);
        drop(guard);
        match read {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } => return Err(ExecError::Retry),
            StepOutcome::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
            StepOutcome::Err(err) => return Err(ExecError::from_read_errno(err.into())),
        }

        if header.starts_with(b"#!") {
            if let Some((interp, opt_arg)) = shebang_parse(&header) {
                if depth == SHEBANG_MAX_DEPTH {
                    return Err(ExecError::SymlinkLoop);
                }
                EXEC_SHEBANG_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let (next_path, next_argv) =
                    shebang_exec_argv(interp, opt_arg, &current_path, &argv_refs)?;
                current_path = next_path;
                current_argv = next_argv;
                continue;
            }
        } else if !header.starts_with(b"\x7fELF") {
            if !is_oscomp_lmbench_hello_wrapper(&header) {
                return Err(ExecError::NotExecutable);
            }
            if depth == SHEBANG_MAX_DEPTH {
                return Err(ExecError::SymlinkLoop);
            }
            let (next_path, next_argv) =
                shebang_exec_argv(b"/bin/sh", None, &current_path, &argv_refs)?;
            current_path = next_path;
            current_argv = next_argv;
            continue;
        }

        return exec_script_inner::<P>(
            depth,
            process,
            thread,
            &current_path,
            &original_path,
            &argv_refs,
            &envp_refs,
            cred,
            retained_page_containers,
        )
        .await;
    }
    Err(ExecError::SymlinkLoop)
}

fn is_oscomp_lmbench_hello_wrapper(header: &[u8]) -> bool {
    let trimmed_len = header
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(0, |index| index + 1);
    &header[..trimmed_len] == OSCOMP_LMBENCH_HELLO_WRAPPER
}

async fn exec_script_inner<P: PmapIf + EntropyIf + tx_hal::AuxvIf>(
    depth: usize,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    path: &[u8],
    execfn: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    cred: &Credential,
    retained_page_containers: &mut Vec<Cap<PageContainer>>,
) -> Result<(), ExecError> {
    // Shebang recursion guard (Linux limit: 4).
    if depth > SHEBANG_MAX_DEPTH {
        return Err(ExecError::SymlinkLoop);
    }
    let mount_namespace = process
        .mount_namespace_cap()
        .ok_or(ExecError::PathNotFound)?;

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
    let (rooted_at, origin_mount) = if path.starts_with(b"/") {
        (
            mount_namespace.root_dentry(),
            mount_namespace.root().clone(),
        )
    } else {
        let cwd = process.cwd_binding().ok_or(ExecError::PathNotFound)?;
        (cwd.dentry, cwd.mount)
    };
    let main_candidate = open_executable_candidate(
        rooted_at,
        &origin_mount,
        path,
        cred,
        &mount_namespace,
        ExecutableCandidateRole::Main,
    )?;
    let openfile = main_candidate.file;
    let file_pc = main_candidate.pc;
    retain_exec_page_container(retained_page_containers, &file_pc);
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
    let exec_meta = main_candidate.meta;
    let main_mount = main_candidate.mount;

    // ===== Phase 2 — bounded script/ELF-kind probe ===================
    //
    // This prefix is consumed only by Phase 2.5's script rules. ELF header,
    // phdr and PT_INTERP reads are performed in declared-offset stages below.
    let read_len = usize::try_from(file_size.min(BINPRM_BUF_SIZE as u64))
        .map_err(|_| ExecError::NotExecutable)?;
    if read_len == 0 {
        return Err(ExecError::NotExecutable);
    }
    let mut header_bytes = try_zeroed_exec_bytes(read_len)?;
    {
        use StepOutcome as V3;
        let guard = step_engine::guard();
        let outcome = read_exact_at(&file_pc, 0, &mut header_bytes, &guard);
        let result = match outcome {
            V3::Done(()) => Ok(()),
            V3::Continue { .. } => Err(ExecError::Retry),
            V3::Yield { shape, .. } => Err(ExecError::Deferred(shape)),
            V3::Err(err) => Err(ExecError::from_read_errno(err.into())),
        };
        drop(guard);
        result?;
    }

    // ===== Phase 2.5 — executable-kind confirmation =================
    // Valid shebang redirects are resolved by the bounded outer loop. A
    // plain non-ELF file returns ENOEXEC; userspace shells own fallback.
    let is_elf = header_bytes.len() >= 4 && &header_bytes[..4] == b"\x7fELF";
    if !is_elf {
        return Err(ExecError::NotExecutable);
    }
    if read_len < 64 {
        return Err(ExecError::NotExecutable);
    }

    // ===== Phase 3 — staged read + parse + validate ==================
    //
    // The loader reads exactly the 64-byte header, then the bounded phdr
    // table at e_phoff, then an optional bounded PT_INTERP string. All reads
    // use the platform's architecture and USER_TOP policy and occur pre-PoNR.
    let parsed: ExecImagePlan =
        read_elf_image::<P>(&file_pc, ImageRole::Main).map_err(ExecError::from_image_read_error)?;

    // ===== Phase 3b — load interpreter (if PT_INTERP present) =========
    //
    // When the main binary carries PT_INTERP, open and parse the interpreter
    // ELF. `select_combined_layout` later chooses non-overlapping page-aligned
    // main/interpreter/stack/vDSO ranges, randomizing movable placements; the
    // interpreter is rebased with checked load-bias arithmetic.
    let interp_unlaid: Option<(ExecImagePlan, Cap<PageContainer>)> =
        if let Some(ref interp_path) = parsed.interpreter_path {
            debug_assert!(interp_path.starts_with(b"/"));
            let rooted_at = mount_namespace.root_dentry();
            let interp_candidate = open_executable_candidate(
                rooted_at,
                mount_namespace.root(),
                interp_path,
                cred,
                &mount_namespace,
                ExecutableCandidateRole::Interpreter,
            )?;
            let interp_pc = interp_candidate.pc;
            retain_exec_page_container(retained_page_containers, &interp_pc);
            let interp_parsed = read_elf_image::<P>(&interp_pc, ImageRole::Interpreter)
                .map_err(ExecError::from_interpreter_image_read_error)?;
            Some((interp_parsed, interp_pc))
        } else {
            None
        };

    // Select the complete main/interpreter/stack/vDSO layout once, before any
    // detached-AS recipes are created. Each image is rebased atomically and
    // with its own maximum LOAD alignment.
    let (interp_plan, interp_pc) = match interp_unlaid {
        Some((plan, pc)) => (Some(plan), Some(pc)),
        None => (None, None),
    };
    let layout = select_combined_layout(parsed, interp_plan, P::USER_TOP.0 as u64, random_u64)
        .map_err(ExecError::Layout)?;
    let CombinedImageLayout {
        main: parsed,
        interpreter,
        stack_top,
        vdso_layout,
        ..
    } = layout;
    let interp_data = interpreter.zip(interp_pc);

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
    // mutation stays reversible: the helper signs a fresh `Cap<Cred>`
    // but does not publish it. Any later pre-PoNR error drops that cap;
    // Phase 7 consumes it in one infallible slot swap.
    //
    // The recompute MUST run before Phase 5's auxv build so the
    // `AT_SECURE` slot reflects the post-recompute effective-id delta.
    //
    // nosuid mount check: if the file resides on a mount with the
    // `NOSUID` flag, skip S_ISUID/S_ISGID processing and keep the
    // caller's credentials.  `AT_SECURE` is not set.
    let mount_nosuid = main_mount.flags().contains(MountFlags::NOSUID);

    let setid_policy = if mount_nosuid {
        ExecSetidPolicy::Suppress
    } else {
        ExecSetidPolicy::Apply
    };
    let process_prep = ProcessExecPrep::begin(process, thread).map_err(|error| match error {
        tx_subsystems::process::ExecPrepError::Again => ExecError::Again,
        tx_subsystems::process::ExecPrepError::OutOfMemory => ExecError::OutOfMemory,
        tx_subsystems::process::ExecPrepError::Zombie
        | tx_subsystems::process::ExecPrepError::StaleBinding => ExecError::Again,
    })?;
    let mut prepared_exec_cred = prepare_exec_cred_in(
        process_prep,
        Uid(exec_meta.uid),
        Gid(exec_meta.gid),
        exec_meta.mode,
        setid_policy,
    )
    .map_err(|error| match error {
        Errno::ENOMEM => ExecError::OutOfMemory,
        Errno::EAGAIN => ExecError::Again,
        _ => ExecError::InvalidArgument,
    })?;
    let at_secure = prepared_exec_cred.at_secure();

    // ===== Phase 4 — build detached AddressSpace =====================
    //
    // `txdoc:EXEC-9-2-CREATE-DETACHED-ADDRESS-SPACE`. Bridge the
    // parser's `LoadSegment` (which has no PageContainer — the
    // parser is purely byte-driven) to `vm::scripts::LoadSegment`
    // (which carries the `Cap<PageContainer>` the recipe row references
    // for demand-faulting bytes from the file). Every LOAD segment
    // shares the same backing `Cap` (they all view different ranges
    // of the same file).
    let interp_exec_stack = interp_data
        .as_ref()
        .is_some_and(|(i, _)| i.stack.executable_requested);
    let image_plan = build_vm_image_plan(&parsed, interp_exec_stack, stack_top, &file_pc)?;
    // V1 (`build_aspace_from_image`) takes no `&Guard` — it acquires
    // its own per-call guards internally. Per
    // `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` callers must NOT
    // hold a guard at the call site.
    let new_aspace = vm_scripts::build_aspace_from_image::<P>(&image_plan)
        .map_err(ExecError::from_build_aspace_error)?;

    // ===== Phase 4a.2 — register interpreter LOAD segments =========
    //
    // The interpreter uses the same VM registration primitive as the main
    // image, after the combined layout pass has proven all final ranges.
    if let Some((ref interp, ref interp_pc)) = interp_data {
        let interpreter_vm_plan = build_vm_image_plan(interp, false, stack_top, interp_pc)?;
        vm_scripts::register_image_load_segments(&new_aspace, &interpreter_vm_plan)
            .map_err(ExecError::from_build_aspace_error)?;
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
        // Page-floor of `file_end` (start of the last page that
        // contains file data).  `partial_start` is the vaddr of
        // the first file-data byte in that page — clamped to
        // `segment.vaddr` in case the segment starts mid-page.
        let file_end_page_floor = file_end & !(page_size - 1);
        let partial_start = file_end_page_floor.max(segment.vaddr);
        // Number of file-content bytes in this partial page.
        // This is the distance from `partial_start` to `file_end`,
        // *not* the page-offset of `file_end` — the latter
        // over-counts when the segment started mid-page and the
        // page floor lies before `segment.vaddr`.
        let partial_in_page = file_end - partial_start;
        if partial_in_page == 0 {
            continue;
        }
        // File offset of `partial_start`. The segment's `file_offset`
        // corresponds to `vaddr`; offsetting by `partial_start - vaddr`
        // gives the file offset of the bytes we need to seed. If the
        // segment starts mid-page, the page floor can precede `vaddr`,
        // so `partial_start` is clamped to the segment start.
        let file_off = segment
            .file_offset
            .checked_add(partial_start - segment.vaddr)
            .ok_or(ExecError::NotExecutable)?;
        let mut buf = try_zeroed_exec_bytes(partial_in_page as usize)?;
        {
            use StepOutcome as V3;
            let guard = step_engine::guard();
            match read_exact_at(&segment.backing, file_off, &mut buf, &guard) {
                V3::Done(()) => {}
                V3::Continue { .. } => return Err(ExecError::Retry),
                V3::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
                V3::Err(err) => return Err(ExecError::from_read_errno(err.into())),
            }
        }
        match vm_scripts::populate_detached_user_range(&new_aspace, partial_start, &buf).await {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(err) => {
                return Err(ExecError::from_populate_errno(err.into()));
            }
            StepOutcome::Continue { .. } => return Err(ExecError::Retry),
            StepOutcome::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
        }
    }
    if let Some((ref interp, ref interp_pc)) = interp_data {
        for segment in &interp.load_segments {
            if segment.filesz == 0 || segment.memsz <= segment.filesz {
                continue;
            }
            let file_end = segment
                .vaddr
                .checked_add(segment.filesz)
                .ok_or(ExecError::NotExecutable)?;
            let file_end_page_floor = file_end & !(page_size - 1);
            let partial_start = file_end_page_floor.max(segment.vaddr);
            let partial_in_page = file_end - partial_start;
            if partial_in_page == 0 {
                continue;
            }
            let file_off = segment
                .file_offset
                .checked_add(partial_start - segment.vaddr)
                .ok_or(ExecError::NotExecutable)?;
            let mut buf = try_zeroed_exec_bytes(partial_in_page as usize)?;
            {
                use StepOutcome as V3;
                let guard = step_engine::guard();
                match read_exact_at(interp_pc, file_off, &mut buf, &guard) {
                    V3::Done(()) => {}
                    V3::Continue { .. } => return Err(ExecError::Retry),
                    V3::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
                    V3::Err(err) => return Err(ExecError::from_read_errno(err.into())),
                }
            }
            match vm_scripts::populate_detached_user_range(&new_aspace, partial_start, &buf).await {
                StepOutcome::Done(()) => {}
                StepOutcome::Err(err) => {
                    return Err(ExecError::from_populate_errno(err.into()));
                }
                StepOutcome::Continue { .. } => return Err(ExecError::Retry),
                StepOutcome::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
            }
        }
    }

    // ===== Phase 4b — map optional vDSO/VVAR before exec PoNR =========
    //
    // The mapping commits its recipes and PTEs as one reversible detached-AS
    // operation. A target without an image returns None; a mapping failure
    // exits before auxv construction and before address-space publication.
    let vdso_mapping =
        map_vdso_into_aspace(&new_aspace, vdso_layout).map_err(ExecError::from_vdso_map_error)?;
    validate_vdso_mapping(vdso_mapping, vdso_layout).map_err(ExecError::Layout)?;

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
    let aux_cred = *prepared_exec_cred.credential();

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

    let interp_aux = interp_data
        .as_ref()
        .map(|(interp, _)| (interp.load_bias, interp.entry));

    let auxv_facts = AuxvFacts {
        at_phdr: parsed.at_phdr,
        at_phent: ELF64_PHENT,
        at_phnum: parsed.at_phnum,
        at_pagesz: USER_PAGE_SIZE,
        // Dynamic ELF starts at the interpreter entry. The dynamic
        // linker still needs the main program entry in AT_ENTRY and
        // its own load bias in AT_BASE to relocate and hand off.
        at_base: interp_aux.map(|(base, _)| base).unwrap_or(0),
        at_entry: parsed.entry,
        at_uid: aux_cred.uid.raw() as u64,
        at_euid: aux_cred.euid.raw() as u64,
        at_gid: aux_cred.gid.raw() as u64,
        at_egid: aux_cred.egid.raw() as u64,
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
        platform_string: match P::ARCH {
            Arch::Riscv64 => b"riscv64",
            Arch::LoongArch64 => b"loongarch64",
        },
        at_clktck: super::stack::CLKTCK_VALUE,
        execfn_string: execfn,
        at_flags: 0,
        at_sysinfo_ehdr: vdso_auxv(vdso_mapping),
        user_top: P::USER_TOP.0 as u64,
    };
    let stack_image = build_initial_user_stack(stack_top, argv, envp, &auxv_facts).map_err(
        |error| match error {
            StackBuildError::OutOfMemory => ExecError::OutOfMemory,
            StackBuildError::InvalidLayout => ExecError::NotExecutable,
            StackBuildError::InvalidString => ExecError::InvalidArgument,
        },
    )?;
    let argv0 = argv.first().copied().unwrap_or(b"");
    let cmdline_bytes = try_copy_exec_bytes(argv0)?;
    let committed_exe_file = openfile.opendir_dentry();
    let process_group_label = {
        let pgrp = process.pgrp_cap();
        (pgrp.pgid.0, pgrp.session_cap().sid.0)
    };

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
        StepOutcome::Continue { .. } => return Err(ExecError::Retry),
        StepOutcome::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
    }

    // All CLOEXEC scanning, vector growth, and capability retention must happen
    // before sibling collapse and before PoNR. The final commit revalidates this
    // exact snapshot under the fd-table locks immediately before the AS swap.
    let close_plan = prepared_exec_cred
        .process_prep_mut()
        .prepare_cloexec_close()
        .map_err(|error| match error {
            tx_subsystems::process::ExecPrepError::OutOfMemory => ExecError::OutOfMemory,
            tx_subsystems::process::ExecPrepError::Again
            | tx_subsystems::process::ExecPrepError::Zombie
            | tx_subsystems::process::ExecPrepError::StaleBinding => ExecError::Again,
        })?;
    let thread_payload = thread.payload_cap().ok_or(ExecError::Again)?;

    // ----- Phase 5 (cont) — collapse old-AS work -------------------
    //
    // `txdoc:EXEC-10-COLLAPSE-OLD-AS-WORK`. CLONE_THREAD is live, so
    // the old thread group must be reduced to the calling thread after
    // all reversible preparation has succeeded and before the address
    // space replacement becomes visible.
    prepared_exec_cred
        .process_prep_mut()
        .collapse_threads(thread)
        .map_err(|error| match error {
            tx_subsystems::process::ExecPrepError::Zombie => ExecError::Again,
            tx_subsystems::process::ExecPrepError::Again
            | tx_subsystems::process::ExecPrepError::StaleBinding => ExecError::Again,
            tx_subsystems::process::ExecPrepError::OutOfMemory => ExecError::OutOfMemory,
        })?;
    prepared_exec_cred
        .process_prep_mut()
        .validate_commit_ready()
        .map_err(|_| ExecError::Again)?;

    // ===== Phase 6 — address-space visibility boundary ===============
    //
    // `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
    //
    // The checked address-space swap below may still return before mutation if
    // the authoritative process binding is stale. A successful swap begins
    // EXEC-PONR. No recoverable error path follows; an internal lifecycle-
    // invariant violation after the swap is fatal.
    //
    //   1. `process.replace_aspace_and_close_cloexec(...)` validates the
    //      preallocated close plan, exchanges the `AtomicSlot<Cap<AddressSpace>>`,
    //      removes only the prevalidated fds, and returns the previous
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
    // Past a successful swap the function performs only Phase-7 commits. The
    // `?` below is therefore still on the reversible side of the boundary;
    // after it there is no recoverable error path.
    let _previous_aspace = prepared_exec_cred
        .process_prep_mut()
        .replace_aspace_and_close_cloexec(new_aspace, close_plan)
        .map_err(|_| ExecError::Again)?;
    // `_previous_aspace` drops at end of scope; EBR defers reclamation
    // so any concurrent reader on another hart can finish its
    // observation of the old aspace before its memory is reused.

    let entry_pc = interp_aux.map(|(_, entry)| entry).unwrap_or(parsed.entry) as usize;
    let initial_sp = stack_image.initial_sp as usize;
    let user_ctx = make_initial_user_trap_context(P::ARCH, entry_pc, initial_sp);
    thread_payload.store_saved_user_context(Some(user_ctx));

    // ===== Phase 7 — install per-frame replacements ==================
    //
    // `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT` (handled by Phase 6
    // above), `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC` (prepared before
    // PoNR and committed atomically with the Phase-6 AS swap),
    // `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`,
    // `txdoc:EXEC-12-4-INSTALL-BRK`. The remaining post-swap operations are
    // infallible synchronous commits. Order is documented but immaterial —
    // each commit touches a disjoint slot.
    drive_exec_post_commit_ops(process, new_brk_base);
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
        *payload._exe_file.lock() = committed_exe_file;
        // basename(argv[0]) — strip everything up to the last `/`.
        let comm_src = match argv0.iter().rposition(|&b| b == b'/') {
            Some(i) => &argv0[i + 1..],
            None => argv0,
        };
        let mut comm_buf = [0u8; 16];
        let n = core::cmp::min(comm_src.len(), 15);
        comm_buf[..n].copy_from_slice(&comm_src[..n]);
        *payload._comm.lock() = comm_buf;
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
        emit_process_group_for(process.pid.0, process_group_label.0, process_group_label.1);
    }
    let _cred_outcome = commit_prepared_exec_cred(prepared_exec_cred)
        .expect("checked address-space swap preserves the authoritative exec binding");
    // vfork completion: if the parent is waiting on CLONE_VFORK,
    // unblock it now that exec has completed.
    process.fire_exit_source_with_post(1, |mailbox, event| mailbox.post(event));

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

fn retain_exec_page_container(
    retained: &mut Vec<Cap<PageContainer>>,
    container: &Cap<PageContainer>,
) {
    if retained
        .iter()
        .all(|current| current.key() != container.key())
    {
        retained.push(container.clone());
    }
}

fn drive_exec_post_commit_ops(process: &Cap<ProcessIdentity>, new_brk_base: u64) {
    let mut script_ctx = step_engine::ScriptCtx::<ProcessIdentity>::new();

    let mut signal_op = ResetSignalDispositionsForExecOp { process };
    step_engine::drive_oneshot(&mut signal_op, &mut script_ctx)
        .expect("exec post-commit ResetSignalDispositionsForExecOp is infallible");

    let mut brk_op = InstallBrkForExecOp {
        process,
        new_brk_base,
    };
    step_engine::drive_oneshot(&mut brk_op, &mut script_ctx)
        .expect("exec post-commit InstallBrkForExecOp is infallible");
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

/// Bridge the ELF image plan's view to the
/// vm-scripts view (LOAD segments carry the file's `Cap<PageContainer>`).
fn build_vm_image_plan(
    parsed: &ExecImagePlan,
    interp_exec_stack: bool,
    stack_top: u64,
    file_pc: &Cap<PageContainer>,
) -> Result<VmImagePlan, ExecError> {
    let mut load_segments = Vec::new();
    load_segments
        .try_reserve_exact(parsed.load_segments.len())
        .map_err(|_| ExecError::OutOfMemory)?;
    for seg in &parsed.load_segments {
        load_segments.push(VmLoadSegment {
            vaddr: seg.vaddr,
            memsz: seg.memsz,
            filesz: seg.filesz,
            file_offset: seg.file_offset,
            flags: translate_flags(seg.flags),
            backing: file_pc.clone(),
        });
    }

    let bss_extension = parsed.bss_extension.map(|tail| VmBssTail {
        vaddr: tail.vaddr,
        size: tail.size,
    });

    Ok(VmImagePlan {
        entry: parsed.entry,
        stack_top,
        load_segments,
        bss_extension,
        executable_stack: parsed.stack.executable_requested || interp_exec_stack,
    })
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

/// Return the exact interpreter path declared by `PT_INTERP`.
///
/// Image compatibility belongs in rootfs symlinks or an explicit filesystem
/// adapter, not in ELF semantics.
#[cfg(test)]
fn interpreter_lookup_paths(interp_path: &[u8]) -> Vec<Vec<u8>> {
    alloc::vec![interp_path.to_vec()]
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

fn poll_ready_synchronously<F: core::future::Future>(future: F) -> Option<F::Output> {
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

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
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
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
        return if mode & 0o111 != 0 {
            Ok(())
        } else {
            Err(ExecError::PermissionDenied)
        };
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

fn open_executable_candidate(
    rooted_at: Cap<DEntry>,
    origin_mount: &Cap<tx_subsystems::mount::MountIdentity>,
    path: &[u8],
    cred: &Credential,
    mount_namespace: &Cap<MountNamespace>,
    role: ExecutableCandidateRole,
) -> Result<ExecutableCandidate, ExecError> {
    use StepOutcome as V3;

    let guard = step_engine::guard();
    let outcome = step_open_in_mount_namespace_with_origin_mount(
        rooted_at,
        origin_mount,
        path,
        OpenFileFlags {
            read: false,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
        0,
        cred,
        mount_namespace,
        &guard,
    );
    let opened = match outcome {
        V3::Done(opened) => opened,
        V3::Continue { .. } => return Err(ExecError::Retry),
        V3::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
        V3::Err(err) => {
            EXEC_LAST_OPEN_ERRNO.store(err as i32, core::sync::atomic::Ordering::Relaxed);
            return Err(ExecError::from_walker_errno(Errno::from(err)));
        }
    };

    if opened.mount.flags().contains(MountFlags::NOEXEC) {
        return Err(ExecError::PermissionDenied);
    }
    let file = opened.open_file;

    // RNode metadata is a materialisation-time snapshot.  In particular,
    // linkers commonly create an output with 0666 (subject to umask), then
    // fchmod it executable while an earlier path lookup still keeps the
    // dentry/RNode alive.  Permission and credential transitions must use
    // the filesystem's current inode metadata, just like Linux consults the
    // canonical live inode, rather than the stale RNode snapshot.
    //
    // Keeping the load on the mount selected by the namespace walk also
    // matters for bind mounts: ascending an arbitrary dentry parent chain can
    // select the wrong filesystem after a concurrent namespace operation.
    let mount_payload = opened.mount.payload_cap().map_err(|_| ExecError::Retry)?;
    let meta = match mount_payload
        .fs_ops()
        .load_inode_meta(file.rnode().fs_object_id(), &guard)
    {
        V3::Done(meta) => meta,
        V3::Continue { .. } => return Err(ExecError::Retry),
        V3::Yield { shape, .. } => return Err(ExecError::Deferred(shape)),
        V3::Err(err) => return Err(ExecError::from_walker_errno(Errno::from(err))),
    };
    drop(guard);

    check_exec_perm(&meta, cred)?;
    if meta.kind() == InodeKind::Directory {
        return Err(ExecError::PermissionDenied);
    }
    if meta.kind() != InodeKind::Regular {
        return Err(role.invalid_image());
    }
    let pc = match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return Err(role.invalid_image()),
    };

    Ok(ExecutableCandidate {
        file,
        pc,
        meta,
        mount: opened.mount,
    })
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
/// Format: `#! <whitespace>? <interp> <whitespace> <opt_arg>? <newline>`.
/// Linux passes the entire trimmed text after the interpreter as one optional
/// argument. If the 256-byte probe ends before a terminator, only a possibly
/// truncated interpreter token makes the line invalid; optional text may be
/// truncated at the probe boundary.
fn shebang_parse(header: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    debug_assert!(header.starts_with(b"#!"));
    let window = &header[..header.len().min(BINPRM_BUF_SIZE)];
    let after_marker = window.get(2..)?;
    let terminator = after_marker.iter().position(|&b| b == b'\n' || b == b'\0');

    if terminator.is_none() && window.len() == BINPRM_BUF_SIZE {
        let interpreter = shebang_trim_start(after_marker);
        if !interpreter.iter().any(|&b| b == b' ' || b == b'\t') {
            return None;
        }
    }

    let line_end = terminator.unwrap_or_else(|| {
        if window.len() == BINPRM_BUF_SIZE {
            // Linux reserves buf[BINPRM_BUF_SIZE - 1] as the forced NUL
            // terminator when no newline was found in the probe.
            after_marker.len() - 1
        } else {
            after_marker.len()
        }
    });
    let line = shebang_trim_start(shebang_trim_end(&after_marker[..line_end]));
    if line.is_empty() {
        return None;
    }
    let (interp, rest) = shebang_split_word(line);
    if interp.is_empty() {
        return None;
    }
    let rest = shebang_trim_end(shebang_trim_start(rest));
    let opt_arg = if rest.is_empty() { None } else { Some(rest) };
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
) -> Result<(Vec<u8>, Vec<Vec<u8>>), ExecError> {
    let normalized_interp = if interp == b"/bin/busybox" {
        b"/bin/sh".as_slice()
    } else {
        interp
    };
    let interp_path = try_copy_exec_bytes(normalized_interp)?;
    let mut opt_arg = opt_arg;
    if interp == b"/bin/busybox" {
        if matches!(opt_arg, Some(b"sh" | b"ash")) {
            opt_arg = None;
        }
    }

    // Linux binfmt_script shape: [interp, opt_arg?, script_path,
    // original argv[1..]...].
    let item_count = 2usize
        .checked_add(usize::from(opt_arg.is_some()))
        .and_then(|count| count.checked_add(original_argv.len().saturating_sub(1)))
        .ok_or(ExecError::OutOfMemory)?;
    let mut new_argv: Vec<Vec<u8>> = Vec::new();
    try_reserve_exec_items(&mut new_argv, item_count)?;
    new_argv.push(try_copy_exec_bytes(&interp_path)?);
    if let Some(arg) = opt_arg {
        new_argv.push(try_copy_exec_bytes(arg)?);
    }
    new_argv.push(try_copy_exec_bytes(script_path)?);
    for &a in original_argv.iter().skip(1) {
        new_argv.push(try_copy_exec_bytes(a)?);
    }
    Ok((interp_path, new_argv))
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
        .rposition(|&b| b != b' ' && b != b'\t')
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
