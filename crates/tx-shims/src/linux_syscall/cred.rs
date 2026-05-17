//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

pub(super) fn decode_uid_arg(raw: u32) -> Option<Uid> {
    if raw == UID_LEAVE_UNCHANGED {
        None
    } else {
        Some(Uid(raw))
    }
}

pub(super) fn decode_gid_arg(raw: u32) -> Option<Gid> {
    if raw == UID_LEAVE_UNCHANGED {
        None
    } else {
        Some(Gid(raw))
    }
}

/// Map a `CredChange` outcome from a setter helper to the dispatched
/// `SyscallResult`. `Replaced` → `Return(0)`; `PermissionDenied` →
/// `-EPERM`; `Zombie` → `-ESRCH` (impossible in practice — the caller
/// is by definition alive — but defensive).
pub(super) fn cred_change_to_result(change: CredChange) -> SyscallResult {
    match change {
        CredChange::Replaced { .. } => SyscallResult::Return(0),
        CredChange::PermissionDenied => SyscallResult::Error(EPERM_VALUE),
        CredChange::Zombie => SyscallResult::Error(ESRCH_VALUE),
    }
}

/// `getuid()`. Linux RV64 generic ABI `__NR_getuid`. Returns the
/// caller's real uid. Reads `ctx.cred()` once; no `.await`, no
/// privilege check (everyone can read their own uid).
pub(super) fn sys_getuid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().uid.raw() as i64)
}

/// `geteuid()`. Linux RV64 generic ABI `__NR_geteuid`. Returns the
/// caller's effective uid.
pub(super) fn sys_geteuid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().euid.raw() as i64)
}

/// `getgid()`. Linux RV64 generic ABI `__NR_getgid`. Returns the
/// caller's real gid.
pub(super) fn sys_getgid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().gid.raw() as i64)
}

/// `getegid()`. Linux RV64 generic ABI `__NR_getegid`. Returns the
/// caller's effective gid.
pub(super) fn sys_getegid<'a>(ctx: &SyscallCtx<'a>) -> SyscallResult {
    SyscallResult::Return(ctx.cred().egid.raw() as i64)
}

/// `setuid(uid)`. Wraps `cred::step_setuid` (Wave 1).
///
/// Privileged callers (`euid == 0` or `CAP_SETUID`) get all four of
/// `uid`, `euid`, `suid` set to `uid`. Non-privileged callers may
/// only swap `euid` among `(uid, euid, suid)`; any other target
/// returns `-EPERM`.
pub(super) fn sys_setuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let target = Uid(args[0] as u32);
    // PR-3: one-shot dispatch via drive_oneshot (no reactor, no yield)
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetuidOp {
        target: ctx.process.clone(),
        new_uid: target,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `setgid(gid)`. Wraps `cred::step_setgid` (Wave 1). Same privilege
/// rules as `setuid` applied to the gid family.
///
/// PR-3 migration: `SetgidOp` is a `OneShotStepOp` — dispatched via
/// `drive_oneshot` (no reactor, no ActiveWait, no yield).
pub(super) fn sys_setgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let target = Gid(args[0] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetgidOp {
        target: ctx.process.clone(),
        new_gid: target,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `setreuid(ruid, euid)`. Wraps `cred::step_setreuid` (Wave 1).
///
/// Each argument: `(u32) -1` (= `u32::MAX`) means "leave unchanged".
/// Privileged callers may set arbitrary values. Non-privileged
/// callers must each (when not the sentinel) supply a value
/// currently in `{uid, euid, suid}`. Linux quirk: when `ruid` is
/// supplied OR the post-call `euid` differs from the pre-call real
/// uid, the saved-set `suid` is bumped to the post-call effective
/// uid (the rule that distinguishes `setreuid` from `setresuid`).
pub(super) fn sys_setreuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid = decode_uid_arg(args[0] as u32);
    let euid = decode_uid_arg(args[1] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetreuidOp {
        target: ctx.process.clone(),
        ruid,
        euid,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `setregid(rgid, egid)`. Gid analog of `sys_setreuid`. Wraps
/// `cred::step_setregid` (Wave 1).
pub(super) fn sys_setregid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid = decode_gid_arg(args[0] as u32);
    let egid = decode_gid_arg(args[1] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetregidOp {
        target: ctx.process.clone(),
        rgid,
        egid,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `setresuid(ruid, euid, suid)`. Wraps `cred::step_setresuid`
/// (Wave 1). Each argument decodes the `(u32) -1` sentinel to
/// `Option::None` ("leave unchanged"). Privileged callers may set
/// any combination. Non-privileged callers must each (when not the
/// sentinel) supply a value currently in `{uid, euid, suid}`; if any
/// one fails the rule, no field changes and the call returns
/// `-EPERM` (atomic per `step_setresuid`'s contract).
pub(super) fn sys_setresuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid = decode_uid_arg(args[0] as u32);
    let euid = decode_uid_arg(args[1] as u32);
    let suid = decode_uid_arg(args[2] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetresuidOp {
        target: ctx.process.clone(),
        ruid,
        euid,
        suid,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `setresgid(rgid, egid, sgid)`. Gid analog of `sys_setresuid`.
/// Wraps `cred::step_setresgid` (Wave 1).
pub(super) fn sys_setresgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid = decode_gid_arg(args[0] as u32);
    let egid = decode_gid_arg(args[1] as u32);
    let sgid = decode_gid_arg(args[2] as u32);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SetresgidOp {
        target: ctx.process.clone(),
        rgid,
        egid,
        sgid,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(change) => cred_change_to_result(change),
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
    }
}

/// `getresuid(ruid_uaddr, euid_uaddr, suid_uaddr)`. Reads
/// `ctx.cred()` once and writes each `u32` raw uid to the
/// corresponding user pointer. NULL pointers skip that write.
///
/// Each uaddr is written through `bootstrap_write_user::<u32>`
/// (canonical `aspace.write_user` lane with kernel-pointer fallback
/// for test scaffolding). NULL pointers skip the write.
pub(super) fn sys_getresuid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ruid_uaddr = args[0];
    let euid_uaddr = args[1];
    let suid_uaddr = args[2];
    let cred = ctx.cred();

    if ruid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, ruid_uaddr, cred.uid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if euid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, euid_uaddr, cred.euid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if suid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, suid_uaddr, cred.suid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

/// `getresgid(rgid_uaddr, egid_uaddr, sgid_uaddr)`. Gid analog of
/// `sys_getresuid`. Same bridging through the user-VA lane applies.
pub(super) fn sys_getresgid<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let rgid_uaddr = args[0];
    let egid_uaddr = args[1];
    let sgid_uaddr = args[2];
    let cred = ctx.cred();

    if rgid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, rgid_uaddr, cred.gid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if egid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, egid_uaddr, cred.egid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if sgid_uaddr != 0 {
        if let Err(errno) = bootstrap_write_user::<u32>(&ctx.aspace, sgid_uaddr, cred.sgid.raw()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}
