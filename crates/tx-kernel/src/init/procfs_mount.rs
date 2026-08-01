// Procfs boot-mount helper.
//
// Carved out of `init.rs` to keep that file under the 1800-line
// per-file authoring cap enforced by the `arch` lint
// (`txdoc:CI-GATE-ARCH-LINT`). The single method here augments the
// `impl<P: TxPlatform> CoreInit<P>` block in `init.rs`.
//
// See [`Self::mount_procfs_at_proc`] for the full design rationale.

use super::*;

impl<P: TxPlatform> CoreInit<P> {
    /// Mount procfs on `/proc`.
    ///
    /// Creates `/proc` on the rootfs (tmpfs) and mounts procfs there so
    /// that userspace tools like `free`, `ps`, and `df` can read
    /// `/proc/meminfo`, `/proc/<pid>/stat`, and `/proc/mounts`.
    ///
    /// **Order invariant:** runs after `mount_rootfs_from_boot_media`.
    pub(crate) fn mount_procfs_at_proc() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("mount_procfs_at_proc: ROOT_MOUNT must be populated");

        let guard = step_engine::guard();
        let cred = Credential::root();
        use StepOutcome as V3;
        let root_fs_object_id = root_mount.root().fs_object_id();
        let (proc_object_id, proc_meta) = match root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .fs_ops
            .mkdir(root_fs_object_id, b"proc", 0o555, &cred, &guard)
        {
            V3::Done(out) => out,
            V3::Err(step_engine::Errno::ENOSYS) | V3::Err(step_engine::Errno::EROFS) => {
                (root_fs_object_id, root_mount.root().meta())
            }
            other => panic!("mount_procfs_at_proc: mkdir(/proc) failed: {other:?}"),
        };
        drop(guard);

        let proc_rnode_in_root = RNode::new_cap(proc_object_id, proc_meta, RNodeBacking::Directory)
            .expect("mount_procfs_at_proc: /proc rnode-on-rootfs reservation");
        let proc_dentry_on_root = DEntry::new_cap(
            InlineName::new(b"proc").expect("mount_procfs_at_proc: /proc inline name"),
            proc_rnode_in_root,
        )
        .expect("mount_procfs_at_proc: /proc dentry-on-rootfs reservation");

        let procfs_fs_ops = tx_fs::procfs::Procfs::fs_ops_arc();
        let procfs_fs_page_backing = alloc::sync::Arc::new(tx_fs::procfs::Procfs)
            as alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>;

        let procfs_payload = MountPayload::new_cap(
            procfs_fs_ops,
            procfs_fs_page_backing,
            None,
            mount::allocate_dev_id(),
            MountOptions::default(),
            "proc",
            SourceLabel::Static("proc"),
        )
        .expect("mount_procfs_at_proc: payload reservation");

        let procfs_root_rnode = {
            let raw = RNode::new(
                tx_fs::procfs::PROCFS_ROOT_ID,
                InodeMeta::new(
                    tx_subsystems::vfs::InodeKind::Directory,
                    tx_fs::procfs::PROCFS_DIR_MODE,
                ),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&procfs_payload);
            let res = step_engine::reserve_for::<RNode>()
                .expect("mount_procfs_at_proc: procfs root rnode reservation");
            step_engine::sign_for(res, raw)
        };

        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();

        let proc_mount = MountIdentity::new_cap(
            mount::allocate_mount_id(),
            Some(proc_dentry_on_root),
            procfs_root_rnode,
            Some(root_mount),
            procfs_payload,
            MountFlags::empty(),
        )
        .expect("mount_procfs_at_proc: mount identity reservation");

        mount::register_mount(&rootfs_payload, proc_object_id, proc_mount);

        crate::init::note_mount_line("proc /proc proc rw 0 0");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":mount:procfs:ok\n");
    }
}
