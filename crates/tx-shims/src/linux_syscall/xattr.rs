//! Extended-attribute syscall arms.
//!
//! Storage belongs to filesystem backends through `FsOps`; this module only
//! decodes the Linux ABI, resolves path/fd targets through the dirfd-aware VFS
//! facade, and copies values to/from user memory.

use super::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tx_subsystems::vfs::FsOps;

#[derive(Clone, Copy)]
struct XattrArgs {
    value: u64,
    size: u32,
    flags: u32,
}

fn read_xattr_name(ctx: &SyscallCtx<'_>, name_uaddr: u64) -> Result<Vec<u8>, SyscallResult> {
    if name_uaddr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    match read_user_cstr(
        &ctx.aspace,
        name_uaddr,
        tx_subsystems::vfs::XATTR_NAME_MAX + 1,
    ) {
        Ok(name) => Ok(name),
        Err(ReadCStrError::TooLong) => Err(SyscallResult::Error(ERANGE_VALUE)),
    }
}

fn read_xattr_path(ctx: &SyscallCtx<'_>, path_uaddr: u64) -> Result<Vec<u8>, SyscallResult> {
    if path_uaddr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    match read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(path) => Ok(path),
        Err(ReadCStrError::TooLong) => Err(SyscallResult::Error(ENAMETOOLONG_VALUE)),
    }
}

fn read_xattr_args(
    ctx: &SyscallCtx<'_>,
    args_uaddr: u64,
    args_size: usize,
) -> Result<XattrArgs, SyscallResult> {
    if args_size < 16 {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    if args_size > USER_PAGE_SIZE {
        return Err(SyscallResult::Error(E2BIG_VALUE));
    }
    let mut raw = [0u8; 16];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut raw, args_uaddr) {
        return Err(SyscallResult::error_from(errno));
    }
    Ok(XattrArgs {
        value: u64::from_le_bytes(raw[0..8].try_into().expect("u64 xattr arg")),
        size: u32::from_le_bytes(raw[8..12].try_into().expect("u32 xattr arg")),
        flags: u32::from_le_bytes(raw[12..16].try_into().expect("u32 xattr arg")),
    })
}

fn validate_at_flags(flags: u32) -> Result<(), SyscallResult> {
    let known = AT_EMPTY_PATH | (AT_SYMLINK_NOFOLLOW as u32);
    if flags & !known != 0 {
        Err(SyscallResult::Error(EINVAL_VALUE))
    } else {
        Ok(())
    }
}

fn xattr_target_for_fd(
    ctx: &SyscallCtx<'_>,
    fd: i32,
) -> Result<(Arc<dyn FsOps>, tx_subsystems::vfs::FsObjectId), SyscallResult> {
    if fd < 0 {
        return Err(SyscallResult::Error(EBADF_VALUE));
    }
    let open_file = ctx
        .process
        .fd(fd as u32)
        .ok_or(SyscallResult::Error(EBADF_VALUE))?;
    let rnode = match open_file.backing() {
        tx_subsystems::vfs::structure::OpenFileBacking::Rnode { rnode } => rnode.clone(),
        _ => return Err(SyscallResult::Error(EBADF_VALUE)),
    };
    let fs_ops = fs_ops_for_rnode(&rnode).ok_or(SyscallResult::Error(ENOSYS_VALUE))?;
    Ok((fs_ops, rnode.fs_object_id()))
}

fn xattr_target_for_empty_path(
    ctx: &SyscallCtx<'_>,
    dirfd: i32,
) -> Result<(Arc<dyn FsOps>, tx_subsystems::vfs::FsObjectId), SyscallResult> {
    if dirfd == AT_FDCWD {
        let cwd = ctx
            .process
            .cwd()
            .ok_or(SyscallResult::Error(ENOENT_VALUE))?;
        let fs_ops = fs_ops_for_dentry(&cwd).ok_or(SyscallResult::Error(ENOSYS_VALUE))?;
        return Ok((fs_ops, cwd.rnode().fs_object_id()));
    }
    xattr_target_for_fd(ctx, dirfd)
}

fn xattr_target_for_path(
    ctx: &SyscallCtx<'_>,
    dirfd: i32,
    path: &[u8],
    at_flags: u32,
) -> Result<(Arc<dyn FsOps>, tx_subsystems::vfs::FsObjectId), SyscallResult> {
    validate_at_flags(at_flags)?;
    if path.is_empty() {
        if at_flags & AT_EMPTY_PATH == 0 {
            return Err(SyscallResult::Error(ENOENT_VALUE));
        }
        return xattr_target_for_empty_path(ctx, dirfd);
    }

    let walker_cred = ctx.walker_cred();
    let request = if at_flags & (AT_SYMLINK_NOFOLLOW as u32) != 0 {
        ResolveRequest::entity(dirfd, path, &walker_cred).nofollow()
    } else {
        ResolveRequest::entity(dirfd, path, &walker_cred)
    };
    let resolved = match drive_resolve(ctx, request) {
        Ok(resolved) => resolved,
        Err(errno) => return Err(SyscallResult::Error(errno)),
    };
    let fs_ops = fs_ops_for_dentry(&resolved.dentry).ok_or(SyscallResult::Error(ENOSYS_VALUE))?;
    Ok((fs_ops, resolved.fs_object_id))
}

fn copy_xattr_value_from_user(
    ctx: &SyscallCtx<'_>,
    value_uaddr: u64,
    size: usize,
) -> Result<Vec<u8>, SyscallResult> {
    if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_value_len(size) {
        return Err(SyscallResult::error_from(errno));
    }
    if size > 0 && value_uaddr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    let mut value = Vec::new();
    value.resize(size, 0);
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut value, value_uaddr) {
        return Err(SyscallResult::error_from(errno));
    }
    Ok(value)
}

fn stage_xattr_output(size: usize, is_list: bool) -> Result<Vec<u8>, SyscallResult> {
    let validation = if is_list {
        tx_subsystems::vfs::xattr::validate_xattr_list_len(size)
    } else {
        tx_subsystems::vfs::xattr::validate_xattr_value_len(size)
    };
    if let Err(errno) = validation {
        return Err(SyscallResult::error_from(errno));
    }
    let mut buf = Vec::new();
    buf.resize(size, 0);
    Ok(buf)
}

fn sys_setxattr_common(
    ctx: &SyscallCtx<'_>,
    fs_ops: Arc<dyn FsOps>,
    fs_object_id: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    value_uaddr: u64,
    size: usize,
    flags: u32,
) -> SyscallResult {
    let value = match copy_xattr_value_from_user(ctx, value_uaddr, size) {
        Ok(value) => value,
        Err(result) => return result,
    };
    let guard = step_engine::guard();
    match fs_ops.set_xattr(
        fs_object_id,
        name,
        &value,
        flags,
        &ctx.walker_cred(),
        &guard,
    ) {
        step_engine::StepOutcome::Done(()) => SyscallResult::Return(0),
        step_engine::StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        step_engine::StepOutcome::Continue { .. } | step_engine::StepOutcome::Yield { .. } => {
            SyscallResult::Error(EIO_VALUE)
        }
    }
}

fn sys_getxattr_common(
    ctx: &SyscallCtx<'_>,
    fs_ops: Arc<dyn FsOps>,
    fs_object_id: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    value_uaddr: u64,
    size: usize,
) -> SyscallResult {
    if size > 0 && value_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let mut value = match stage_xattr_output(size, false) {
        Ok(value) => value,
        Err(result) => return result,
    };
    let guard = step_engine::guard();
    match fs_ops.get_xattr(fs_object_id, name, &mut value, &ctx.walker_cred(), &guard) {
        step_engine::StepOutcome::Done(required) => {
            if required != 0 {
                if let Err(errno) =
                    bootstrap_copy_to_user(&ctx.aspace, value_uaddr, &value[..required])
                {
                    return SyscallResult::error_from(errno);
                }
            }
            SyscallResult::Return(required as i64)
        }
        step_engine::StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        step_engine::StepOutcome::Continue { .. } | step_engine::StepOutcome::Yield { .. } => {
            SyscallResult::Error(EIO_VALUE)
        }
    }
}

fn sys_listxattr_common(
    ctx: &SyscallCtx<'_>,
    fs_ops: Arc<dyn FsOps>,
    fs_object_id: tx_subsystems::vfs::FsObjectId,
    list_uaddr: u64,
    size: usize,
) -> SyscallResult {
    if size > 0 && list_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let mut list = match stage_xattr_output(size, true) {
        Ok(list) => list,
        Err(result) => return result,
    };
    let guard = step_engine::guard();
    match fs_ops.list_xattr(fs_object_id, &mut list, &ctx.walker_cred(), &guard) {
        step_engine::StepOutcome::Done(required) => {
            if required != 0 {
                if let Err(errno) =
                    bootstrap_copy_to_user(&ctx.aspace, list_uaddr, &list[..required])
                {
                    return SyscallResult::error_from(errno);
                }
            }
            SyscallResult::Return(required as i64)
        }
        step_engine::StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        step_engine::StepOutcome::Continue { .. } | step_engine::StepOutcome::Yield { .. } => {
            SyscallResult::Error(EIO_VALUE)
        }
    }
}

fn sys_removexattr_common(
    ctx: &SyscallCtx<'_>,
    fs_ops: Arc<dyn FsOps>,
    fs_object_id: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
) -> SyscallResult {
    let guard = step_engine::guard();
    match fs_ops.remove_xattr(fs_object_id, name, &ctx.walker_cred(), &guard) {
        step_engine::StepOutcome::Done(()) => SyscallResult::Return(0),
        step_engine::StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        step_engine::StepOutcome::Continue { .. } | step_engine::StepOutcome::Yield { .. } => {
            SyscallResult::Error(EIO_VALUE)
        }
    }
}

pub(super) fn sys_setxattr_path(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    nofollow: bool,
) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[0]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let at_flags = if nofollow {
        AT_SYMLINK_NOFOLLOW as u32
    } else {
        0
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, AT_FDCWD, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_setxattr_common(
        ctx,
        fs_ops,
        fs_object_id,
        &name,
        args[2],
        args[3] as usize,
        args[4] as u32,
    )
}

pub(super) fn sys_fsetxattr(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_fd(ctx, args[0] as i32) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_setxattr_common(
        ctx,
        fs_ops,
        fs_object_id,
        &name,
        args[2],
        args[3] as usize,
        args[4] as u32,
    )
}

pub(super) fn sys_getxattr_path(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    nofollow: bool,
) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[0]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let at_flags = if nofollow {
        AT_SYMLINK_NOFOLLOW as u32
    } else {
        0
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, AT_FDCWD, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_getxattr_common(ctx, fs_ops, fs_object_id, &name, args[2], args[3] as usize)
}

pub(super) fn sys_fgetxattr(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_fd(ctx, args[0] as i32) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_getxattr_common(ctx, fs_ops, fs_object_id, &name, args[2], args[3] as usize)
}

pub(super) fn sys_listxattr_path(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    nofollow: bool,
) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[0]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let at_flags = if nofollow {
        AT_SYMLINK_NOFOLLOW as u32
    } else {
        0
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, AT_FDCWD, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_listxattr_common(ctx, fs_ops, fs_object_id, args[1], args[2] as usize)
}

pub(super) fn sys_flistxattr(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (fs_ops, fs_object_id) = match xattr_target_for_fd(ctx, args[0] as i32) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_listxattr_common(ctx, fs_ops, fs_object_id, args[1], args[2] as usize)
}

pub(super) fn sys_removexattr_path(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    nofollow: bool,
) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[0]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let at_flags = if nofollow {
        AT_SYMLINK_NOFOLLOW as u32
    } else {
        0
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, AT_FDCWD, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_removexattr_common(ctx, fs_ops, fs_object_id, &name)
}

pub(super) fn sys_fremovexattr(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let name = match read_xattr_name(ctx, args[1]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_fd(ctx, args[0] as i32) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_removexattr_common(ctx, fs_ops, fs_object_id, &name)
}

pub(super) fn sys_setxattrat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[1]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let at_flags = args[2] as u32;
    let name = match read_xattr_name(ctx, args[3]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let xargs = match read_xattr_args(ctx, args[4], args[5] as usize) {
        Ok(args) => args,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, args[0] as i32, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_setxattr_common(
        ctx,
        fs_ops,
        fs_object_id,
        &name,
        xargs.value,
        xargs.size as usize,
        xargs.flags,
    )
}

pub(super) fn sys_getxattrat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[1]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let at_flags = args[2] as u32;
    let name = match read_xattr_name(ctx, args[3]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let xargs = match read_xattr_args(ctx, args[4], args[5] as usize) {
        Ok(args) => args,
        Err(result) => return result,
    };
    if xargs.flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let (fs_ops, fs_object_id) = match xattr_target_for_path(ctx, args[0] as i32, &path, at_flags) {
        Ok(target) => target,
        Err(result) => return result,
    };
    sys_getxattr_common(
        ctx,
        fs_ops,
        fs_object_id,
        &name,
        xargs.value,
        xargs.size as usize,
    )
}

pub(super) fn sys_listxattrat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[1]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) =
        match xattr_target_for_path(ctx, args[0] as i32, &path, args[2] as u32) {
            Ok(target) => target,
            Err(result) => return result,
        };
    sys_listxattr_common(ctx, fs_ops, fs_object_id, args[3], args[4] as usize)
}

pub(super) fn sys_removexattrat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let path = match read_xattr_path(ctx, args[1]) {
        Ok(path) => path,
        Err(result) => return result,
    };
    let name = match read_xattr_name(ctx, args[3]) {
        Ok(name) => name,
        Err(result) => return result,
    };
    let (fs_ops, fs_object_id) =
        match xattr_target_for_path(ctx, args[0] as i32, &path, args[2] as u32) {
            Ok(target) => target,
            Err(result) => return result,
        };
    sys_removexattr_common(ctx, fs_ops, fs_object_id, &name)
}
