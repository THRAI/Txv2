//! Minimal Linux file-handle syscalls for LTP's VFS handle tests.
//!
//! The handle is txKernel-local: a typed 16-byte payload containing the
//! in-mount `FsObjectId` plus a magic word. It is stable enough for
//! `name_to_handle_at` -> `open_by_handle_at` within one boot, which is
//! the contract the current tmpfs-backed LTP cases exercise.

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::vfs::structure::RNode;
use tx_subsystems::vfs::FsObjectId;

const MAX_HANDLE_BYTES: u32 = 128;
const TXV2_HANDLE_BYTES: u32 = 16;
const TXV2_HANDLE_TYPE: i32 = 1;
const TXV2_HANDLE_MAGIC: u64 = 0x5458_5632_4648_0001;
const AT_SYMLINK_FOLLOW_U32: u32 = 0x400;
const KNOWN_NAME_TO_HANDLE_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_FOLLOW_U32;
const EOVERFLOW_VALUE: i32 = 75;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct FileHandleHeader {
    handle_bytes: u32,
    handle_type: i32,
}

fn errno_result(errno: Errno) -> SyscallResult {
    SyscallResult::Error(errno_to_i32(errno))
}

fn read_handle_header(
    ctx: &SyscallCtx<'_>,
    handle_uaddr: u64,
) -> Result<FileHandleHeader, SyscallResult> {
    bootstrap_read_user::<FileHandleHeader>(&ctx.aspace, handle_uaddr)
        .map_err(|errno| SyscallResult::Error(errno_to_i32(errno)))
}

fn write_handle_header(
    ctx: &SyscallCtx<'_>,
    handle_uaddr: u64,
    header: FileHandleHeader,
) -> Result<(), SyscallResult> {
    bootstrap_write_user::<FileHandleHeader>(&ctx.aspace, handle_uaddr, header)
        .map_err(|errno| SyscallResult::Error(errno_to_i32(errno)))
}

fn handle_payload_addr(handle_uaddr: u64) -> Result<u64, SyscallResult> {
    handle_uaddr
        .checked_add(core::mem::size_of::<FileHandleHeader>() as u64)
        .ok_or(SyscallResult::Error(EFAULT_VALUE))
}

fn write_handle_payload(
    ctx: &SyscallCtx<'_>,
    handle_uaddr: u64,
    fs_object_id: FsObjectId,
) -> Result<(), SyscallResult> {
    let mut payload = [0u8; TXV2_HANDLE_BYTES as usize];
    payload[..8].copy_from_slice(&fs_object_id.as_u64().to_le_bytes());
    payload[8..].copy_from_slice(&TXV2_HANDLE_MAGIC.to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, handle_payload_addr(handle_uaddr)?, &payload)
        .map_err(|errno| SyscallResult::Error(errno_to_i32(errno)))
}

fn read_handle_payload(
    ctx: &SyscallCtx<'_>,
    handle_uaddr: u64,
) -> Result<FsObjectId, SyscallResult> {
    let mut payload = [0u8; TXV2_HANDLE_BYTES as usize];
    bootstrap_copy_from_user(
        &ctx.aspace,
        &mut payload,
        handle_payload_addr(handle_uaddr)?,
    )
    .map_err(|errno| SyscallResult::Error(errno_to_i32(errno)))?;

    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&payload[..8]);
    let mut magic_bytes = [0u8; 8];
    magic_bytes.copy_from_slice(&payload[8..]);
    if u64::from_le_bytes(magic_bytes) != TXV2_HANDLE_MAGIC {
        return Err(errno_result(Errno::ESTALE));
    }
    Ok(FsObjectId::new(u64::from_le_bytes(id_bytes)))
}

fn payload_for_rnode(rnode: &Cap<RNode>) -> Option<Cap<MountPayload>> {
    let guard = step_engine::guard();
    rnode.containing_mount_weak()?.upgrade(&guard)
}

fn mount_payload_from_fd(
    mount_fd: i32,
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<MountPayload>, SyscallResult> {
    if mount_fd == AT_FDCWD {
        let cwd = ctx
            .process
            .cwd()
            .ok_or(SyscallResult::Error(ENOENT_VALUE))?;
        return payload_for_rnode(cwd.rnode()).ok_or_else(|| errno_result(Errno::ESTALE));
    }
    if mount_fd < 0 {
        return Err(SyscallResult::Error(EBADF_VALUE));
    }
    let file = ctx
        .process
        .fd(mount_fd as u32)
        .ok_or(SyscallResult::Error(EBADF_VALUE))?;
    payload_for_rnode(file.rnode()).ok_or_else(|| errno_result(Errno::ESTALE))
}

fn load_meta_from_payload(
    payload: &Cap<MountPayload>,
    fs_object_id: FsObjectId,
) -> Result<InodeMeta, SyscallResult> {
    let guard = step_engine::guard();
    match payload.fs_ops.load_inode_meta(fs_object_id, &guard) {
        StepOutcome::Done(meta) => Ok(meta),
        StepOutcome::Err(errno) => Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            Err(SyscallResult::Error(EIO_VALUE))
        }
    }
}

fn resolve_empty_path_target(
    dfd: i32,
    ctx: &SyscallCtx<'_>,
) -> Result<(FsObjectId, InodeMeta), SyscallResult> {
    let rnode: Cap<RNode> = if dfd == AT_FDCWD {
        ctx.process
            .cwd()
            .ok_or(SyscallResult::Error(ENOENT_VALUE))?
            .rnode()
            .clone()
    } else if dfd < 0 {
        return Err(SyscallResult::Error(EBADF_VALUE));
    } else {
        ctx.process
            .fd(dfd as u32)
            .ok_or(SyscallResult::Error(EBADF_VALUE))?
            .rnode()
            .clone()
    };

    let fs_object_id = rnode.fs_object_id();
    let meta = match payload_for_rnode(&rnode) {
        Some(payload) => load_meta_from_payload(&payload, fs_object_id)?,
        None => rnode.meta(),
    };
    Ok((fs_object_id, meta))
}

fn resolve_nofollow_target(
    root: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
) -> Result<(FsObjectId, InodeMeta), SyscallResult> {
    let (parent_path, basename) = split_path(path);
    if basename.is_empty() {
        return Err(SyscallResult::Error(ENOENT_VALUE));
    }
    let parent = if parent_path.is_empty() {
        root
    } else {
        walk_from(root, parent_path, cred).map_err(SyscallResult::Error)?
    };
    let fs_ops = fs_ops_for_dentry(&parent).ok_or(SyscallResult::Error(EROFS_VALUE))?;
    let parent_id = parent.rnode().fs_object_id();
    let guard = step_engine::guard();
    let target_id = match fs_ops.lookup(parent_id, basename, &guard) {
        StepOutcome::Done(id) => id,
        StepOutcome::Err(errno) => return Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return Err(SyscallResult::Error(EIO_VALUE));
        }
    };
    let meta = match fs_ops.load_inode_meta(target_id, &guard) {
        StepOutcome::Done(meta) => meta,
        StepOutcome::Err(errno) => return Err(SyscallResult::error_from(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return Err(SyscallResult::Error(EIO_VALUE));
        }
    };
    Ok((target_id, meta))
}

fn resolve_path_target(
    dfd: i32,
    path: &[u8],
    flags: u32,
    ctx: &SyscallCtx<'_>,
) -> Result<(FsObjectId, InodeMeta), SyscallResult> {
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SyscallResult::Error(ENOENT_VALUE));
        }
        return resolve_empty_path_target(dfd, ctx);
    }

    let root = resolve_cwd_for_path(dfd, path, ctx).map_err(SyscallResult::Error)?;
    let cred = ctx.walker_cred();
    if flags & AT_SYMLINK_FOLLOW_U32 != 0 {
        let dentry = walk_from(root, path, &cred).map_err(SyscallResult::Error)?;
        Ok((dentry.rnode().fs_object_id(), dentry.rnode().meta()))
    } else {
        resolve_nofollow_target(root, path, &cred)
    }
}

fn resolve_cwd_for_path(
    dirfd: i32,
    _path: &[u8],
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    if dirfd == AT_FDCWD {
        return ctx.process.cwd().ok_or(ENOENT_VALUE);
    }
    if dirfd < 0 {
        return Err(EBADF_VALUE);
    }
    let open_file = ctx.process.fd(dirfd as u32).ok_or(EBADF_VALUE)?;
    open_file.opendir_dentry().ok_or(ENOTDIR_VALUE)
}

pub(super) fn sys_name_to_handle_at<P: PmapIf>(
    dfd: i32,
    path_uaddr: u64,
    handle_uaddr: u64,
    mount_id_uaddr: u64,
    flags: u32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    if flags & !KNOWN_NAME_TO_HANDLE_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if path_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let path = match bootstrap_read_user_cstr(&ctx.aspace, path_uaddr, EXECVE_PATH_MAX) {
        Ok(path) => path,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let header = match read_handle_header(ctx, handle_uaddr) {
        Ok(header) => header,
        Err(err) => return err,
    };
    if header.handle_bytes > MAX_HANDLE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let (fs_object_id, _meta) = match resolve_path_target(dfd, &path, flags, ctx) {
        Ok(target) => target,
        Err(err) => return err,
    };

    if let Err(errno) = bootstrap_write_user::<i32>(&ctx.aspace, mount_id_uaddr, 1) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let out_header = FileHandleHeader {
        handle_bytes: TXV2_HANDLE_BYTES,
        handle_type: TXV2_HANDLE_TYPE,
    };
    if let Err(err) = write_handle_header(ctx, handle_uaddr, out_header) {
        return err;
    }
    if header.handle_bytes < TXV2_HANDLE_BYTES {
        return SyscallResult::Error(EOVERFLOW_VALUE);
    }
    if let Err(err) = write_handle_payload(ctx, handle_uaddr, fs_object_id) {
        return err;
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_open_by_handle_at(
    mount_fd: i32,
    handle_uaddr: u64,
    flags: u32,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let header = match read_handle_header(ctx, handle_uaddr) {
        Ok(header) => header,
        Err(err) => return err,
    };
    if header.handle_bytes == 0 || header.handle_bytes > MAX_HANDLE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if header.handle_bytes < TXV2_HANDLE_BYTES || header.handle_type != TXV2_HANDLE_TYPE {
        return errno_result(Errno::ESTALE);
    }
    if !ctx
        .cred()
        .effective_caps
        .contains(Capability::DAC_READ_SEARCH)
    {
        return SyscallResult::Error(EPERM_VALUE);
    }

    let fs_object_id = match read_handle_payload(ctx, handle_uaddr) {
        Ok(id) => id,
        Err(err) => return err,
    };
    let mount_payload = match mount_payload_from_fd(mount_fd, ctx) {
        Ok(payload) => payload,
        Err(err) => return err,
    };
    let meta = match load_meta_from_payload(&mount_payload, fs_object_id) {
        Ok(meta) => meta,
        Err(err) => return err,
    };
    if meta.kind() == InodeKind::Symlink {
        return errno_result(Errno::ELOOP);
    }

    let (read, write) = decode_access_mode(flags);
    let open_flags = OpenFileFlags {
        read,
        write,
        append: flags & O_APPEND != 0,
        cloexec: flags & O_CLOEXEC != 0,
        nonblocking: flags & O_NONBLOCK != 0,
        packet: false,
    };

    let rnode = if meta.kind() == InodeKind::Directory {
        match RNode::new_cap_in_mount(fs_object_id, meta, RNodeBacking::Directory, &mount_payload) {
            Ok(rnode) => rnode,
            Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
        }
    } else {
        let guard = step_engine::guard();
        match mount_payload
            .fs_ops
            .materialise_rnode(fs_object_id, meta, &mount_payload, &guard)
        {
            StepOutcome::Done(rnode) => rnode,
            StepOutcome::Err(errno) => return SyscallResult::error_from(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    };

    let open_file = match OpenFile::new_cap(rnode, open_flags) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let fd = match next_stdio_fd_below_nofile(&ctx.process) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.set_fd(fd, Some(open_file));
    if open_flags.cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }
    SyscallResult::Return(fd as i64)
}
