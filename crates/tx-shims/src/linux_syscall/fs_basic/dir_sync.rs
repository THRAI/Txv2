use super::*;

pub(in crate::linux_syscall) async fn sys_getdents64<'a>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_uaddr = args[1];
    let buf_len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if buf_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // Only directory backings produce dirents. Pipes, regular files,
    // TTYs, and chardevs surface `-ENOTDIR` per Linux's
    // `man getdents64`.
    let dir_fs_object_id = match file.rnode().backing() {
        RNodeBacking::Directory => file.rnode().fs_object_id(),
        _ => return SyscallResult::Error(ENOTDIR_VALUE),
    };

    // Resolve the FsOps for this directory's mount. The OpenFile's
    // rnode is the same `Cap<RNode>` that the walker installed via
    // step_open against a mount-published dentry — we can't pull the
    // mount payload directly off the rnode (`materialise_child_rnode`
    // doesn't carry the mount weak), so reuse the dentry-side
    // `fs_ops_for_dentry` shape via a synthetic dentry. In practice
    // every directory rnode this path sees is the mount root or a
    // descendant materialised through step_open, and the rnode
    // itself carries `containing_mount_weak()` only when it *is* the
    // mount root. For descendants we fall through to `None` below
    // and the call surfaces -ENOSYS defensively. tmpfs's directory
    // tree uses a single rnode-per-inode with the mount weak set
    // only at the root, so this is the practical limit today.
    //
    // TODO(phase-readdir-mount): teach `materialise_child_rnode` to
    // forward the mount weak so descendants don't hit the fallback.
    // Until then, every test fixture uses the mount-root directory.
    let fs_ops = match fs_ops_for_rnode(file.rnode()) {
        Some(o) => o,
        None => return SyscallResult::Error(ENOSYS_VALUE),
    };

    let mut cursor = file.readdir_cursor();
    let mut written: usize = 0;

    use StepOutcome as DirOutcome;
    loop {
        let outcome = {
            let guard = step_engine::guard();
            fs_ops.readdir(dir_fs_object_id, cursor, &guard)
        };
        match outcome {
            DirOutcome::Done(Some((entry, next_cursor))) => {
                let name_bytes = entry.name.as_bytes();
                let raw_len = LINUX_DIRENT64_HEADER_BYTES + name_bytes.len() + 1;
                let total_len = align_up_8(raw_len);
                if written + total_len > buf_len {
                    if written == 0 {
                        // Even the first record didn't fit — caller's
                        // buffer is too small. Linux's
                        // `man getdents64` returns EINVAL here.
                        return SyscallResult::Error(EINVAL_VALUE);
                    }
                    // Stop short; the cursor points at this entry so
                    // the next call resumes here.
                    file.set_readdir_cursor(cursor);
                    break;
                }
                // Build the record image in kernel memory, then copy
                // out through the canonical user-VA lane.
                let mut record: alloc::vec::Vec<u8> = alloc::vec![0u8; total_len];
                let header = LinuxDirent64Header {
                    d_ino: entry.fs_object_id.as_u64(),
                    d_off: next_cursor.as_u64() as i64,
                    d_reclen: total_len as u16,
                    d_type: inode_kind_to_dt(entry.kind),
                };
                // Copy header bytes via `as_bytes` proxy. The header
                // is `repr(C)` and Copy; we transmute through a slice.
                {
                    // SAFETY: header is a valid `#[repr(C)] Copy`
                    // struct whose byte image we want to splice into
                    // the staging Vec. Using `from_raw_parts` against
                    // a stack value keeps the read inside our kernel
                    // memory.
                    let header_bytes = unsafe {
                        core::slice::from_raw_parts(
                            &header as *const LinuxDirent64Header as *const u8,
                            LINUX_DIRENT64_HEADER_BYTES,
                        )
                    };
                    record[..LINUX_DIRENT64_HEADER_BYTES].copy_from_slice(header_bytes);
                }
                record[LINUX_DIRENT64_HEADER_BYTES..LINUX_DIRENT64_HEADER_BYTES + name_bytes.len()]
                    .copy_from_slice(name_bytes);
                // NUL terminator after name; remaining padding bytes
                // already zero from `vec![0; total_len]`.
                if let Err(errno) = bootstrap_copy_to_user(
                    &ctx.aspace,
                    buf_uaddr.wrapping_add(written as u64),
                    &record,
                ) {
                    if written > 0 {
                        return SyscallResult::Return(written as i64);
                    }
                    return SyscallResult::error_from(errno);
                }
                written += total_len;
                cursor = next_cursor;
                file.set_readdir_cursor(cursor);
            }
            DirOutcome::Done(None) => {
                // End of directory — durable cursor advance is
                // unnecessary (the readdir backend's cursor is
                // self-terminating).
                break;
            }
            DirOutcome::Continue { .. } | DirOutcome::Yield { .. } => {
                // No in-tree backend produces these. Surface as
                // `-EIO` defensively if the partial-progress shape
                // ever fires.
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::Error(EIO_VALUE);
            }
            DirOutcome::Err(errno) => {
                if written > 0 {
                    return SyscallResult::Return(written as i64);
                }
                return SyscallResult::error_from(Errno::from(errno));
            }
        }
    }

    SyscallResult::Return(written as i64)
}

/// Resolve the `Arc<dyn FsOps>` in scope for a directory rnode.
/// Mirrors `fs_ops_for_dentry`'s shape (in `fs_path.rs`) but
/// operates on the rnode directly — the OpenFile carries `Cap<RNode>`,
/// not `Cap<DEntry>`.
///
/// Returns `None` if the rnode does not carry a `containing_mount`
/// weak (descendant rnodes minted by `materialise_child_rnode` don't
/// — only mount-root rnodes do). The Slice 6 `getdents64` arm
/// surfaces this as `-ENOSYS` defensively (no `FsOps` to dispatch
/// through). In practice every tested directory is the mount root,
/// matching tmpfs's day-1 surface.
///
/// TODO(phase-readdir-mount): forward the mount weak to descendants
/// during `materialise_child_rnode` so this fallback is unnecessary.
pub(in crate::linux_syscall) fn fs_ops_for_rnode(
    rnode: &Cap<tx_subsystems::vfs::structure::RNode>,
) -> Option<Arc<dyn tx_subsystems::vfs::FsOps>> {
    MountedNode::from_rnode_direct(rnode).map(|mounted| mounted.fs_ops())
}

/// `statfs(path, buf)`. Linux RV64 ABI `__NR_statfs = 43`.
pub(in crate::linux_syscall) async fn sys_statfs<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let buf_uaddr = args[1];
    let statfs = StatfsLayout {
        f_type: 0x0102_1994,
        f_bsize: 4096,
        f_blocks: 1024,
        f_bfree: 768,
        f_bavail: 768,
        f_files: 4096,
        f_ffree: 2048,
        f_fsid: [0, 0],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [0; 4],
    };
    if let Err(e) = bootstrap_write_user::<StatfsLayout>(&ctx.aspace, buf_uaddr, statfs) {
        return SyscallResult::error_from(e);
    }
    SyscallResult::Return(0)
}

/// `fstatfs(fd, buf)`. Linux RV64 ABI `__NR_fstatfs = 44`.
pub(in crate::linux_syscall) async fn sys_fstatfs<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    if ctx.process.fd(fd).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    sys_statfs::<P>(args, ctx).await
}

/// `sync()`. Linux RV64 ABI `__NR_sync = 81`.
pub(in crate::linux_syscall) async fn sys_sync<P: PmapIf>(
    _args: [u64; 6],
    _ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    SyscallResult::Return(0)
}

/// `syncfs(fd)`. Linux RV64 ABI `__NR_syncfs = 267`.
/// Syncs the filesystem containing the given fd.
pub(in crate::linux_syscall) async fn sys_syncfs<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let rnode = open_file.rnode();
    let page_backing = match MountedNode::from_rnode_direct(rnode) {
        Some(mounted) => mounted.fs_page_backing(),
        None => return SyscallResult::Error(ENODEV_VALUE),
    };
    let guard = step_engine::guard();
    // syncfs: flush the entire filesystem. The default impl falls back
    // to `fsync_file(ROOT)`; journaling filesystems can override.
    match page_backing.sync_filesystem(&guard) {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(e) => SyscallResult::error_from(Errno::from(e)),
        _ => SyscallResult::Error(EIO_VALUE),
    }
}

/// `fsync(fd)`. Linux RV64 ABI `__NR_fsync = 82`.
/// Syncs the specific file referenced by `fd` (data + metadata).
pub(in crate::linux_syscall) async fn sys_fsync<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let rnode = open_file.rnode();
    let fs_object_id = rnode.fs_object_id();
    let page_container = crate::linux_syscall::vm::extract_page_container(&open_file);
    // `MountedNode` scopes the mount-weak upgrade to the helper call and
    // returns a cloned page-backing handle, so no guard crosses the subsequent
    // `drive(...).await` (INVARIANTS_v5 EBR-7).
    let page_backing = {
        match MountedNode::from_rnode_direct(rnode) {
            Some(mounted) => mounted.fs_page_backing(),
            None => return SyscallResult::Error(ENODEV_VALUE),
        }
    };
    // fsync: sync the specific file via FileFsyncOp + drive().
    use step_engine::DriveMode;
    use tx_scripts::drive;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_registrar_handle = script_ctx.timer_registrar().cloned();
    let op = FileFsyncOp {
        page_backing,
        fs_object_id,
        page_container,
        state: tx_subsystems::page_backed::FileFsyncState::new(),
    };
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        None,
        timer_registrar_handle.as_ref(),
    )
    .await
    {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// `fdatasync(fd)`. Linux RV64 ABI `__NR_fdatasync = 83`.
/// Syncs file data (not metadata).  v1: delegates to fsync.
pub(in crate::linux_syscall) async fn sys_fdatasync<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    sys_fsync::<P>(args, ctx).await
}

/// `flock(fd, operation)`. Linux RV64 ABI `__NR_flock = 32`.
///
/// v1: exclusive-lock only per open-file-description.  LOCK_SH
/// maps to LOCK_EX.  No deadlock detection.  Per POSIX flock
/// semantics (advisory, not enforced on I/O).
pub(in crate::linux_syscall) async fn sys_flock<P: PmapIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let _ = core::marker::PhantomData::<P>;
    let fd = args[0] as u32;
    let operation = args[1] as u32;

    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_UN: u32 = 8;
    const LOCK_NB: u32 = 4;

    let open_file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if (operation & LOCK_UN) != 0 {
        open_file.flock_release();
        return SyscallResult::Return(0);
    }

    let lock_type = operation & 3; // LOCK_SH=1 or LOCK_EX=2
    if lock_type != LOCK_SH && lock_type != LOCK_EX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let blocking = (operation & LOCK_NB) == 0;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = FlockOp {
        file: &open_file,
        lock_type,
        blocking,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}
