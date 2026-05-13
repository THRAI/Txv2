//! `sys_userfaultfd(2)` + `UFFDIO_API` ioctl scaffold — PR-10 phase 2.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
//!   §6 (phase plan row P-10.2)
//! - `docs/Txv3/05_DELEGATE_v1.md` §8.1 (userfaultfd worked example)
//! - `man 2 userfaultfd`, `man 2 ioctl_userfaultfd`
//!
//! # What lands here
//!
//! 1. [`sys_userfaultfd`] — the syscall dispatcher for
//!    `__NR_userfaultfd = 282`. Allocates a fresh `Cap<UserfaultFd>`
//!    (W-Q's phase 0 zone), wraps it in an `OpenFile` whose backing
//!    is `OpenFileBacking::Ufd`, installs at the lowest free fd via
//!    [`ProcessIdentity::install_fd`], and returns the fd. Recognised
//!    `flags`: `O_CLOEXEC`; other bits return `-EINVAL`.
//!
//! 2. [`step_uffdio_api`] — minimum `UFFDIO_API` ioctl handling:
//!    a no-op handshake that validates the user-provided
//!    `struct uffdio_api { api, features, ioctls }`, writes back a
//!    zero supported-features bitmap, marks the ufd's handshake bit,
//!    and returns `0`. Later phases (P-10.3 / P-10.5) populate the
//!    real ioctl bitmap; today the agent only needs the
//!    "uffdio_api passed" gate to succeed.
//!
//! # Constraints
//!
//! - **No agent integration yet** (phase 4/5 territory). The
//!   handshake does not arm a fault path; it only stamps the
//!   substrate-side bit.
//! - **`UFFDIO_API` is sticky-single-shot** per Linux: a second
//!   handshake against an already-handshaken ufd returns `EPERM`
//!   (we map this from
//!   [`UserfaultFd::mark_api_handshake_done`] returning `false`).
//! - **VM fault path is untouched** (phase 4 territory).

use tx_subsystems::execution::Errno;
use tx_subsystems::userfaultfd::{UfdRange, UserfaultFd};
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::{UfdRegistration, UserRange, UserVirtAddr, VmMapError, USER_PAGE_SIZE};

use super::numbers::{
    O_CLOEXEC, O_NONBLOCK, UFFDIO_REGISTER_MODE_MINOR, UFFDIO_REGISTER_MODE_MISSING,
    UFFDIO_REGISTER_MODE_WP, UFFDIO_REGISTER_REPLY_IOCTLS, UFFD_API,
};
use crate::adapter::step_engine::{self as step_engine, DelegateReply, TransitionOutcome, UfdReply};
use super::{
    bootstrap_read_user, bootstrap_write_user, errno_to_i32, SyscallCtx, SyscallResult,
    EAGAIN_VALUE, EBADF_VALUE, EINVAL_VALUE, ENOMEM_VALUE,
};

/// Userland layout of `struct uffdio_range` (Linux generic uapi
/// `<linux/userfaultfd.h>`). 16-byte POD nested inside
/// [`UffdioRegister`]. Read by `step_uffdio_register` as the
/// `range` field of the agent-supplied
/// `struct uffdio_register { range, mode, ioctls }`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioRange {
    /// Start of the user-VA range to register, page-aligned.
    pub start: u64,
    /// Length in bytes, a multiple of the user page size.
    pub len: u64,
}

/// Userland layout of `struct uffdio_register` (Linux generic uapi
/// `<linux/userfaultfd.h>`):
///
/// ```text
/// struct uffdio_register {
///     struct uffdio_range range;   // in: range to register
///     __u64 mode;                  // in: UFFDIO_REGISTER_MODE_* bits
///     __u64 ioctls;                // out: bitmap of supported UFFDIO_*
///                                  //      ioctls on this range
/// };
/// ```
///
/// 32 bytes, plain `#[repr(C)]` POD — directly `bootstrap_read_user`
/// / `bootstrap_write_user` compatible. The 32-byte size matches the
/// `_IOWR('U', 0x00, struct uffdio_register)` magic encoded in
/// [`super::numbers::UFFDIO_REGISTER`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioRegister {
    pub range: UffdioRange,
    pub mode: u64,
    pub ioctls: u64,
}

/// Userland layout of `struct uffdio_api` (Linux generic uapi
/// `<linux/userfaultfd.h>`):
///
/// ```text
/// struct uffdio_api {
///     __u64 api;       // in: requested api version (must be UFFD_API == 0xAA)
///     __u64 features;  // in: requested feature mask; out: supported mask
///     __u64 ioctls;    // out: bitmap of supported UFFDIO_* ioctls
/// };
/// ```
///
/// Plain `#[repr(C)]` POD struct of three `u64` fields — directly
/// `bootstrap_read_user` / `bootstrap_write_user` compatible. The
/// 24-byte size matches the `_IOWR('U', 0x3F, struct uffdio_api)`
/// magic encoded in [`UFFDIO_API`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioApi {
    pub api: u64,
    pub features: u64,
    pub ioctls: u64,
}

/// `userfaultfd(flags)` syscall arm.
///
/// Per `man 2 userfaultfd`:
/// - `flags`: `O_CLOEXEC` and/or `O_NONBLOCK`. Phase 2 honours
///   `O_CLOEXEC` (sets the per-process cloexec bit on the returned
///   fd) and recognises but does not yet plumb `O_NONBLOCK` — there
///   is no read-fault queue to make non-blocking until P-10.5.
/// - Returns the new fd on success or `-EINVAL` on bad flags / `-EMFILE`
///   if the fd-table is exhausted (mapped to `ENOMEM` here since the
///   substrate does not yet expose RLIMIT_NOFILE).
///
/// **Constraints on this scaffold**:
/// - Caller credentials are not gated (Linux's `CAP_SYS_PTRACE`
///   requirement defers to PR-10 phase 5+ when the registration
///   surface is real).
/// - The returned fd's `OpenFile` carries `OpenFileBacking::Ufd` with
///   no VFS RNode — every VFS syscall (`read`/`write`/`lseek`/etc.)
///   that hits a ufd fd today still panics on the legacy
///   `OpenFile::rnode()` path. Phase 5 wires the read-fault-message
///   path; for the scaffold the agent only needs `ioctl(UFFDIO_API)`
///   and `close()` to succeed.
pub(super) fn sys_userfaultfd<'a>(flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    // Validate flags. PR-10 phase 5 wires the read-fault queue, so
    // `O_NONBLOCK` is now honoured in addition to `O_CLOEXEC`.
    let recognised = O_CLOEXEC | O_NONBLOCK;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let cloexec = (flags & O_CLOEXEC) != 0;
    let nonblocking = (flags & O_NONBLOCK) != 0;

    // Mint a fresh `Cap<UserfaultFd>` via the W-Q phase 0 zone. The
    // `flags` argument is stashed on the payload so future phases can
    // observe it without rederiving from `OpenFileFlags`.
    let ufd_cap = match UserfaultFd::new_with_flags_cap(flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Wrap in an `OpenFile`. `OpenFileFlags::cloexec` is preserved so
    // a later `fcntl(F_GETFD)` / `dup` / `exec`-close-on-exec walk
    // observes the same bit the syscall layer set on the fd-table
    // sidecar.
    let open_flags = OpenFileFlags {
        read: true,
        write: true,
        append: false,
        cloexec,
        nonblocking,
    };
    let open_cap = match OpenFile::new_userfaultfd_cap(ufd_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Install at the lowest free fd.
    let fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(fd, open_cap);
    if cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

/// `UFFDIO_API` ioctl: minimum no-op handshake. Driven from
/// `sys_ioctl(fd, UFFDIO_API, argp)` when the resolved `OpenFile` is
/// a ufd shape.
///
/// Behaviour (PR-10 phase 2):
/// 1. Read the `struct uffdio_api` from `argp`.
/// 2. Reject (`-EINVAL`) if `api != UFFD_API` or `features != 0`
///    (Linux's "api mismatch / unknown feature" path).
/// 3. CAS-set the ufd's handshake bit; on collision return `-EPERM`
///    per Linux's "API already set" rule.
/// 4. Write back `features = 0`, `ioctls = 0` (no supported ioctls
///    yet — `UFFDIO_REGISTER` lands in P-10.3, `UFFDIO_COPY` in
///    P-10.5; the agent uses this round to learn what's available).
/// 5. Return `0`.
pub(super) fn step_uffdio_api(file: &OpenFile, argp: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    // Resolve the ufd payload — `step_uffdio_api` is only reached
    // after `sys_ioctl` has verified the fd is a ufd shape, so this
    // is total in practice. Defensive `EBADF` for the unreachable
    // case.
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if argp == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }

    // Read the uffdio_api struct from userspace.
    let mut api_struct: UffdioApi = match bootstrap_read_user::<UffdioApi>(&ctx.aspace, argp) {
        Ok(s) => s,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    // Validate the api version + features mask. Phase 2 only knows
    // about `UFFD_API == 0xAA` with `features == 0`.
    if api_struct.api != UFFD_API {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if api_struct.features != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // CAS-mark the handshake bit on the ufd payload. `Cap<T>` implements
    // `Deref<Target = T>` (zone-cap deref panics if the slot has been
    // retired — impossible while the cap is held), so the method call
    // resolves directly.
    if !ufd_cap.mark_api_handshake_done() {
        // Already handshaken — Linux returns EPERM.
        return SyscallResult::Error(super::EPERM_VALUE);
    }

    // Write back the supported-features + supported-ioctls bitmaps.
    // Phase 2 ships zero for both — later phases populate as the
    // ioctl arms land.
    api_struct.features = 0;
    api_struct.ioctls = 0;
    if let Err(errno) = bootstrap_write_user::<UffdioApi>(&ctx.aspace, argp, api_struct) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    SyscallResult::Return(0)
}

/// `UFFDIO_REGISTER` ioctl. PR-10 phase 3.
///
/// Driven from `sys_ioctl(fd, UFFDIO_REGISTER, argp)` when the
/// resolved `OpenFile` is a ufd shape. Registers an aligned user-VA
/// range with the ufd so the phase-4 fault path can route faults to
/// the agent.
///
/// Behaviour (per `man ioctl_userfaultfd`, MISSING-mode subset):
/// 1. **API-handshake gate.** The ufd must have completed
///    `UFFDIO_API` first; if not, return `-EINVAL` (matches Linux's
///    "must call UFFDIO_API before UFFDIO_REGISTER" rule).
/// 2. **Read** the user-supplied `struct uffdio_register` from
///    `argp`.
/// 3. **Validate mode.** Phase 3 accepts only
///    `UFFDIO_REGISTER_MODE_MISSING`; `WP` and `MINOR` (and any
///    unknown bits) return `-EINVAL`.
/// 4. **Validate range.** The range must be page-aligned, non-empty,
///    fit in user-VA, and overflow-free.
/// 5. **Tag VMAs.** Call
///    [`tx_subsystems::vm::AddressSpace::tag_ufd_registration`] to
///    stamp every covering VMA with the ufd's id + mode bits. Phase
///    3 requires every VMA in the range to be fully contained
///    (whole-VMA registration); partial overlap returns `-EINVAL`
///    (the substrate's [`VmMapError::MissingMapping`]).
/// 6. **Record** the registration on the ufd payload via
///    [`UserfaultFd::record_registration`].
/// 7. **Write back** the supported reply-ioctls bitmap
///    (`UFFDIO_COPY | UFFDIO_ZEROPAGE`) to the agent.
/// 8. Return `0`.
///
/// **Idempotency.** Re-registering an already-tagged range succeeds:
/// the VMA tag is re-stamped (overwriting the previous tag with the
/// same ufd id + mode) and a fresh entry is appended to the ufd's
/// `registrations_snapshot()`. Linux's behaviour for duplicate
/// `UFFDIO_REGISTER` is "succeeds but is a no-op" against the same
/// owner — phase 3 mirrors that on the VMA tag side (replacing the
/// tag value is a no-op for a same-id same-mode re-register) and
/// keeps the per-ufd append-history for diagnostic purposes.
///
/// **Constraints on this scaffold.**
/// - No fault interception (phase 4 territory).
/// - No `UFFDIO_UNREGISTER` (deferred).
/// - Phase 3 does not split VMAs at the registered range's bounds;
///   the agent must register whole VMAs (the common case).
pub(super) fn step_uffdio_register(
    file: &OpenFile,
    argp: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Linux requires `UFFDIO_API` first.
    if !ufd_cap.api_handshake_done() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if argp == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }

    let mut reg: UffdioRegister = match bootstrap_read_user::<UffdioRegister>(&ctx.aspace, argp) {
        Ok(s) => s,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    // Validate mode bits. Phase 3 accepts MISSING only; any unknown
    // bits or the WP/MINOR bits return EINVAL.
    let known_modes =
        UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP | UFFDIO_REGISTER_MODE_MINOR;
    if reg.mode == 0 || (reg.mode & !known_modes) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (reg.mode & (UFFDIO_REGISTER_MODE_WP | UFFDIO_REGISTER_MODE_MINOR)) != 0 {
        // Per D7 §5 (P-10.3 scope): WP / MINOR deferred.
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (reg.mode & UFFDIO_REGISTER_MODE_MISSING) == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Validate range — page-aligned, non-zero, no overflow, fits in
    // user-VA. `usize::try_from` covers the 32-bit-target case
    // (unreachable for v1 RV64-only targets, but keeps the cast
    // honest).
    let start_usize = match usize::try_from(reg.range.start) {
        Ok(v) => v,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    let len_usize = match usize::try_from(reg.range.len) {
        Ok(v) => v,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    if len_usize == 0
        || !start_usize.is_multiple_of(USER_PAGE_SIZE)
        || !len_usize.is_multiple_of(USER_PAGE_SIZE)
    {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    // Bound check against the v1 user-VA cap (matches AddressSpace's
    // `UserRange::full_user_v1` window). `UserRange::new_aligned`
    // also checks for overflow, but we surface a clean EINVAL on
    // out-of-window starts before paying the substrate call.
    const FULL_USER_V1_TOP: usize = 1 << 38;
    let end_usize = match start_usize.checked_add(len_usize) {
        Some(v) => v,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if end_usize > FULL_USER_V1_TOP {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let user_range = match UserRange::new_aligned(UserVirtAddr(start_usize), len_usize) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Tag covering VMAs. Missing mappings / partial overlap → EINVAL
    // (matches Linux semantics for an unmapped or unaligned range).
    let tag = UfdRegistration {
        ufd_id: ufd_cap.ufd_id(),
        mode: reg.mode,
    };
    match ctx.aspace.tag_ufd_registration(user_range, tag) {
        Ok(_commit) => {}
        Err(VmMapError::MissingMapping) => return SyscallResult::Error(EINVAL_VALUE),
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    }

    // Pin the per-ufd bookkeeping. Phase 3 stores raw range bytes
    // alongside the agent-supplied mode for diagnostic/round-trip
    // tests; later phases use this list to filter `UFFDIO_UNREGISTER`
    // or to drain registrations on close.
    ufd_cap.record_registration(UfdRange {
        start: reg.range.start,
        len: reg.range.len,
        mode: reg.mode,
    });

    // Write back the supported reply-ioctls bitmap so the agent
    // learns what reply primitives it may use against this range.
    // Phase 5 ships `UFFDIO_COPY | UFFDIO_ZEROPAGE | UFFDIO_CONTINUE`.
    reg.ioctls = UFFDIO_REGISTER_REPLY_IOCTLS;
    if let Err(errno) = bootstrap_write_user::<UffdioRegister>(&ctx.aspace, argp, reg) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    SyscallResult::Return(0)
}

// ---------------------------------------------------------------------
// PR-10 phase 5 — UFFDIO_COPY / UFFDIO_ZEROPAGE / UFFDIO_CONTINUE
// ---------------------------------------------------------------------
//
// Driven from `sys_ioctl(fd, UFFDIO_{COPY,ZEROPAGE,CONTINUE}, argp)`
// when the resolved `OpenFile` is a ufd shape. The userspace agent has
// already read a fault message via `read(uffd_fd, &mut uffd_msg)`
// (`step_ufd_read`) and learned the faulting address; the agent now
// invokes one of these three ioctls to satisfy the fault. Each handler:
//
// 1. Validates the UFFDIO_API handshake completed.
// 2. Reads the userland arg struct from `argp`.
// 3. Validates `dst` is page-aligned, `len > 0` and a page-multiple,
//    and falls inside a registered range.
// 4. Looks up the pending fault token via
//    `UserfaultFd::front_fault_msg` matching `dst == fault_addr`. The
//    Linux model has no explicit token field on `struct uffdio_*` — the
//    fault address is the natural identifier, and Linux's userfaultfd
//    delivers one fault at a time per ufd (no overlap on the same
//    address before reply). Mismatch → -EINVAL.
// 5. Calls `ufd.delegate_registry().mark_replied(token_id,
//    DelegateReply::Ufd(...))` with the per-ioctl reply payload.
// 6. Pops the front message off the pending queue (the agent has
//    consumed it).
// 7. Writes back the `copy` / `zeropage` / `mapped` field with the
//    number of bytes installed.
// 8. Returns 0 on success.
//
// **Page copy stub (per W-Y's phase-4 stub semantics).** The actual
// `src` → `dst` byte move for UFFDIO_COPY is deferred to a follow-up;
// the wiring (validation, token resolution, mark_replied, queue pop)
// is what phase 5 pins. The same applies to UFFDIO_CONTINUE's
// page-cache mapping work.

/// Userland layout of `struct uffdio_copy` (Linux generic uapi):
///
/// ```text
/// struct uffdio_copy {
///     __u64 dst;   // in: destination user-VA, page-aligned
///     __u64 src;   // in: kernel-side source buffer (or a user-space
///                  //     buffer in Linux's relaxed model)
///     __u64 len;   // in: bytes to copy, page-multiple
///     __u64 mode;  // in: UFFDIO_COPY_MODE_* bits (DONTWAKE, WP)
///     __u64 copy;  // out: bytes successfully copied (or negated errno
///                  //      on partial failure)
/// };
/// ```
///
/// 40 bytes, matches `_IOWR('U', 0x03, ...) = 0xC028_AA03` (size 0x28).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioCopy {
    pub dst: u64,
    pub src: u64,
    pub len: u64,
    pub mode: u64,
    pub copy: u64,
}

/// Userland layout of `struct uffdio_zeropage`:
///
/// ```text
/// struct uffdio_zeropage {
///     struct uffdio_range range;  // in: dst range, page-aligned
///     __u64 mode;                 // in: UFFDIO_ZEROPAGE_MODE_DONTWAKE
///     __u64 zeropage;             // out: bytes successfully zeroed
/// };
/// ```
///
/// 32 bytes, matches `_IOWR('U', 0x04, ...) = 0xC020_AA04` (size 0x20).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioZeropage {
    pub range: UffdioRange,
    pub mode: u64,
    pub zeropage: u64,
}

/// Userland layout of `struct uffdio_continue`:
///
/// ```text
/// struct uffdio_continue {
///     struct uffdio_range range;  // in: dst range, page-aligned
///     __u64 mode;                 // in: UFFDIO_CONTINUE_MODE_DONTWAKE
///     __u64 mapped;               // out: bytes successfully mapped
/// };
/// ```
///
/// 32 bytes, matches `_IOWR('U', 0x07, ...) = 0xC020_AA07` (size 0x20).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct UffdioContinue {
    pub range: UffdioRange,
    pub mode: u64,
    pub mapped: u64,
}

/// Common validation for the three reply ioctls. Validates the
/// API-handshake gate, page alignment of `dst` + `len`, that the
/// range is fully inside a registered range, and that the ufd's
/// pending-fault queue front carries a `fault_addr == dst`. On
/// success returns the matched `DelegateTokenId` so the caller can
/// run `mark_replied` and pop the message.
fn validate_and_match_pending(
    ufd: &UserfaultFd,
    dst: u64,
    len: u64,
) -> Result<step_engine::DelegateTokenId, i32> {
    if !ufd.api_handshake_done() {
        return Err(EINVAL_VALUE);
    }
    if len == 0 {
        return Err(EINVAL_VALUE);
    }
    let page = USER_PAGE_SIZE as u64;
    if !dst.is_multiple_of(page) || !len.is_multiple_of(page) {
        return Err(EINVAL_VALUE);
    }
    let end = match dst.checked_add(len) {
        Some(v) => v,
        None => return Err(EINVAL_VALUE),
    };

    // Must intersect a registered range.
    let regs = ufd.registrations_snapshot();
    let in_range = regs.iter().any(|r| {
        let r_end = r.start.saturating_add(r.len);
        dst >= r.start && end <= r_end
    });
    if !in_range {
        return Err(EINVAL_VALUE);
    }

    // Look up the front pending message. Linux's userfaultfd delivers
    // one fault at a time per address, so the agent's reply for `dst`
    // is satisfied by the front-most fault whose `fault_addr` equals
    // `dst` (the address the agent received via `step_ufd_read`).
    let front = ufd.front_fault_msg().ok_or(EINVAL_VALUE)?;
    if front.fault_addr != dst {
        return Err(EINVAL_VALUE);
    }
    Ok(front.token_id)
}

/// Map a `TransitionOutcome` from `mark_replied` to a syscall result.
/// `Applied` → success; `LateNoOp` → `-EINVAL` (the token was already
/// terminal — agent's reply came in too late, e.g. fault was cancelled
/// by the script side); `UnknownToken` → `-EINVAL` (programmer error,
/// the queue lookup should always agree with the registry).
fn map_transition(outcome: TransitionOutcome) -> Result<(), i32> {
    match outcome {
        TransitionOutcome::Applied => Ok(()),
        TransitionOutcome::LateNoOp(_) | TransitionOutcome::UnknownToken => Err(EINVAL_VALUE),
    }
}

/// `UFFDIO_COPY` ioctl. PR-10 phase 5.
///
/// **Page-copy stub (carried over from W-Y's phase-4 stub).** The
/// actual `[src, src+len)` → `[dst, dst+len)` byte move is deferred;
/// phase 5 pins the agent → substrate wiring. `src` is read and
/// stashed on the reply payload so the future page-copy work has the
/// kernel-side source pointer to consume.
///
/// Flow:
/// 1. API-handshake gate.
/// 2. Read `struct uffdio_copy` from `argp`.
/// 3. Validate `dst` page-alignment, `len > 0` + page-multiple, range
///    fully covered by a registered range, and front pending fault
///    matches.
/// 4. `mark_replied(token_id, DelegateReply::Ufd(UfdReply::Copy { ...
///    }))`.
/// 5. Pop the matched pending message off the queue.
/// 6. Write back `copy = len` (whole-range success — phase 5 reply
///    payloads do not support partial copy).
pub(super) fn step_uffdio_copy(file: &OpenFile, argp: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if argp == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }
    let mut req: UffdioCopy = match bootstrap_read_user::<UffdioCopy>(&ctx.aspace, argp) {
        Ok(s) => s,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    let token_id = match validate_and_match_pending(ufd_cap, req.dst, req.len) {
        Ok(id) => id,
        Err(code) => return SyscallResult::Error(code),
    };

    let reply = DelegateReply::Ufd(UfdReply::Copy {
        src_kernel_addr: req.src,
        dst_uaddr: req.dst,
        len: req.len,
    });
    if let Err(code) = map_transition(ufd_cap.delegate_registry().mark_replied(token_id, reply)) {
        return SyscallResult::Error(code);
    }

    // Drain the matched fault from the queue. After `mark_replied`
    // returned `Applied` the agent has officially handled the fault;
    // the front message is no longer pending.
    let _ = ufd_cap.pop_fault_msg();

    // Whole-range success — phase 5 does not support partial copy.
    req.copy = req.len;
    if let Err(errno) = bootstrap_write_user::<UffdioCopy>(&ctx.aspace, argp, req) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `UFFDIO_ZEROPAGE` ioctl. PR-10 phase 5. See [`step_uffdio_copy`]
/// for the flow — same shape, payload variant is `UfdReply::ZeroPage`.
pub(super) fn step_uffdio_zeropage(
    file: &OpenFile,
    argp: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if argp == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }
    let mut req: UffdioZeropage = match bootstrap_read_user::<UffdioZeropage>(&ctx.aspace, argp) {
        Ok(s) => s,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    let token_id = match validate_and_match_pending(ufd_cap, req.range.start, req.range.len) {
        Ok(id) => id,
        Err(code) => return SyscallResult::Error(code),
    };

    let reply = DelegateReply::Ufd(UfdReply::ZeroPage {
        dst_uaddr: req.range.start,
        len: req.range.len,
    });
    if let Err(code) = map_transition(ufd_cap.delegate_registry().mark_replied(token_id, reply)) {
        return SyscallResult::Error(code);
    }
    let _ = ufd_cap.pop_fault_msg();

    req.zeropage = req.range.len;
    if let Err(errno) = bootstrap_write_user::<UffdioZeropage>(&ctx.aspace, argp, req) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `UFFDIO_CONTINUE` ioctl. PR-10 phase 5. See [`step_uffdio_copy`]
/// — same shape, payload variant is `UfdReply::Continue`.
pub(super) fn step_uffdio_continue(
    file: &OpenFile,
    argp: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if argp == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }
    let mut req: UffdioContinue = match bootstrap_read_user::<UffdioContinue>(&ctx.aspace, argp) {
        Ok(s) => s,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    let token_id = match validate_and_match_pending(ufd_cap, req.range.start, req.range.len) {
        Ok(id) => id,
        Err(code) => return SyscallResult::Error(code),
    };

    let reply = DelegateReply::Ufd(UfdReply::Continue {
        dst_uaddr: req.range.start,
        len: req.range.len,
    });
    if let Err(code) = map_transition(ufd_cap.delegate_registry().mark_replied(token_id, reply)) {
        return SyscallResult::Error(code);
    }
    let _ = ufd_cap.pop_fault_msg();

    req.mapped = req.range.len;
    if let Err(errno) = bootstrap_write_user::<UffdioContinue>(&ctx.aspace, argp, req) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    SyscallResult::Return(0)
}

/// `read(uffd_fd, buf, len)` arm. PR-10 phase 5.
///
/// Drains one [`tx_subsystems::userfaultfd::UffdMsg`] from the
/// per-ufd pending-fault queue and serializes it into `[buf,
/// buf+UFFD_MSG_WIRE_SIZE)` (32 bytes). Returns:
///
/// - `Return(32)` on a successful single-message drain.
/// - `Error(EINVAL)` if `len < UFFD_MSG_WIRE_SIZE`.
/// - `Error(EAGAIN)` if the queue is empty and the ufd was opened
///   with `O_NONBLOCK`.
/// - Otherwise parks on the per-ufd wait source (via
///   `wait_source::wait_on_token`) and re-polls when a fault is
///   pushed.
pub(super) async fn step_ufd_read(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let ufd_cap = match file.ufd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }
    if len < tx_subsystems::userfaultfd::UFFD_MSG_WIRE_SIZE {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let nonblocking = file.flags().nonblocking;
    use tx_subsystems::execution::WaitToken;
    let wire_size = tx_subsystems::userfaultfd::UFFD_MSG_WIRE_SIZE;
    loop {
        let outcome = {
            let mut staging = [0u8; 32];
            let result = tx_subsystems::userfaultfd::step_ufd_read(
                ufd_cap,
                &mut staging[..wire_size],
                nonblocking,
            );
            (result, staging)
        };
        use step_engine::{StepOutcome as V3Out, YieldShape};
        match outcome.0 {
            V3Out::Done(read) => {
                if read == 0 {
                    return SyscallResult::Return(0);
                }
                if let Err(errno) =
                    super::bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &outcome.1[..read])
                {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                return SyscallResult::Return(read as i64);
            }
            V3Out::Err(v3errno) => {
                let errno: Errno = v3errno.into();
                if errno == Errno::EAGAIN {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
            V3Out::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                let token = WaitToken::new(carrier.raw(), interests.raw());
                if let Some(future) = super::wait_source::wait_on_token(token) {
                    let _ = future.await;
                }
                // Re-poll on next loop iteration.
            }
            // Other shapes are unreachable for the ufd read path.
            V3Out::Continue { .. } | V3Out::Yield { .. } => {
                return SyscallResult::Error(errno_to_i32(Errno::EIO));
            }
        }
    }
}
