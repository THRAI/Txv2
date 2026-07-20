// Sysfs boot-mount helper.
//
// Mirrors the procfs boot mount: mounts the read-only sysfs projection
// (tx_fs::sysfs) on `/sys` so userspace can read
// `/sys/class/net/<ifname>/{address,mtu,...}`. On real Linux `/sys` is a
// sysfs mounted at boot; LTP's net command tests (`tst_init_iface`) read the
// iface MAC/MTU from there, and the lhost reads happen in the root netns
// where the per-test `mount -t sysfs none /sys` (run in the child netns) never
// reaches. Mounting at boot in the root netns makes the class/net view always
// available.

use super::*;

impl<P: TxPlatform> CoreInit<P> {
    /// Mount sysfs on `/sys`.
    ///
    /// **Order invariant:** runs after `mount_rootfs_from_boot_media`.
    pub(crate) fn mount_sysfs_at_sys() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("mount_sysfs_at_sys: ROOT_MOUNT must be populated");

        let guard = step_engine::guard();
        let cred = Credential::root();
        use StepOutcome as V3;
        let root_fs_object_id = root_mount.root().fs_object_id();
        let (sys_object_id, sys_meta) = match root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .fs_ops
            .mkdir(root_fs_object_id, b"sys", 0o555, &cred, &guard)
        {
            V3::Done(out) => out,
            V3::Err(step_engine::Errno::ENOSYS) | V3::Err(step_engine::Errno::EROFS) => {
                (root_fs_object_id, root_mount.root().meta())
            }
            other => panic!("mount_sysfs_at_sys: mkdir(/sys) failed: {other:?}"),
        };
        drop(guard);

        let sys_rnode_in_root = RNode::new_cap(sys_object_id, sys_meta, RNodeBacking::Directory)
            .expect("mount_sysfs_at_sys: /sys rnode-on-rootfs reservation");
        let sys_dentry_on_root = DEntry::new_cap(
            InlineName::new(b"sys").expect("mount_sysfs_at_sys: /sys inline name"),
            sys_rnode_in_root,
        )
        .expect("mount_sysfs_at_sys: /sys dentry-on-rootfs reservation");

        let sysfs_fs_ops = tx_fs::sysfs::Sysfs::fs_ops_arc();
        let sysfs_fs_page_backing = tx_fs::sysfs::Sysfs::fs_page_backing_arc();

        let sysfs_payload = MountPayload::new_cap(
            sysfs_fs_ops,
            sysfs_fs_page_backing,
            None,
            mount::allocate_dev_id(),
            MountOptions::default(),
            "sysfs",
            SourceLabel::Static("sysfs"),
        )
        .expect("mount_sysfs_at_sys: payload reservation");

        let sysfs_root_rnode = {
            let raw = RNode::new(
                tx_fs::sysfs::SYSFS_ROOT_ID,
                InodeMeta::new(
                    tx_subsystems::vfs::InodeKind::Directory,
                    tx_fs::sysfs::SYSFS_DIR_MODE,
                ),
                RNodeBacking::Directory,
            )
            .with_containing_mount(&sysfs_payload);
            let res = step_engine::reserve_for::<RNode>()
                .expect("mount_sysfs_at_sys: sysfs root rnode reservation");
            step_engine::sign_for(res, raw)
        };

        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();

        let sys_mount = MountIdentity::new_cap(
            mount::allocate_mount_id(),
            Some(sys_dentry_on_root),
            sysfs_root_rnode,
            Some(root_mount),
            sysfs_payload,
            MountFlags::empty(),
        )
        .expect("mount_sysfs_at_sys: mount identity reservation");

        mount::register_mount(&rootfs_payload, sys_object_id, sys_mount);

        crate::init::note_mount_line("sysfs /sys sysfs rw 0 0");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":mount:sysfs:ok\n");
    }
}
