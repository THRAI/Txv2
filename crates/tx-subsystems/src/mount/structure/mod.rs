use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicU8};

use tx_substrate::bus::RawPort;
use tx_substrate::epoch::Guard;
use tx_substrate::zone::{Cap, PayloadCap, Zone, ZoneAllocated};

use crate::page_backed::{Frame, FsPageBacking};
use crate::step::StepOutcome;
use crate::vfs::fs_ops::FsOps;
use crate::vfs::structure::{DEntry, DEntryKey};

pub struct MountIdentity {
    pub mountpoint: Cap<DEntry>,
    pub root_dentry: Cap<DEntry>,
    pub parent: MountParentSlot,
    pub mnt_ns: MountNamespaceBinding,
    pub children: MountChildren,
    pub child_chain: MountChildLink,
    pub payload: MountPayloadBinding,
    pub flags: MountFlags,
    pub propagation: AtomicU8,
    pub umount_port: UmountPort,
}

pub enum MountParent {
    Root,
    Attached(MountParentBinding),
    Detached,
}

pub struct MountParentSlot {
    _private: (),
}

pub struct MountParentBinding {
    pub parent: Cap<MountIdentity>,
}

pub struct MountPayload {
    pub payload_pin_count: AtomicU32,
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub backing: Option<Arc<dyn BlockDevice>>,
    pub dev_id: DevId,
    pub options: MountOptions,
    pub fstype: &'static str,
    pub source_label: SourceLabel,
}

pub enum MountPayloadBinding {
    Pending,
    Attached(PayloadCap<MountPayload>),
    Detached,
}

pub struct MountPayloadPin {
    pub payload: Cap<MountPayload>,
}

impl MountPayloadPin {
    pub fn acquire(payload: &Cap<MountPayload>) -> Self {
        payload
            .payload_pin_count
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        Self {
            payload: payload.clone(),
        }
    }
}

impl Drop for MountPayloadPin {
    fn drop(&mut self) {
        self.payload
            .payload_pin_count
            .fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
    }
}

pub struct MountNamespace {
    pub root_mount: Cap<MountIdentity>,
    pub mountpoint_index: MountpointIndex,
    pub all_mounts: AllMounts,
}

pub struct MountpointIndex {
    pub entries: [Option<MountpointEntry>; 64],
}

pub struct MountpointEntry {
    pub key: DEntryKey,
    pub mount: Cap<MountIdentity>,
}

pub struct MountNamespaceBinding {
    pub namespace: Cap<MountNamespace>,
}

pub struct MountChildren {
    _private: (),
}

pub struct MountChildLink {
    _private: (),
}

pub struct AllMounts {
    _private: (),
}

pub struct UmountPort {
    pub port: RawPort,
}

pub enum UmountEvent {
    Detached,
}

pub trait BlockDevice: Send + Sync + 'static {
    fn read_blocks<'g>(
        &self,
        block_id: PhysicalBlockNumber,
        count: u32,
        target: &mut [Frame],
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn write_blocks<'g>(
        &self,
        block_id: PhysicalBlockNumber,
        count: u32,
        source: &[Frame],
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn barrier<'g>(&self, guard: &'g Guard<'g>) -> StepOutcome<()>;
    fn total_blocks(&self) -> u64;
    fn block_size(&self) -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalBlockNumber(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MountId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevId(pub u32);

pub struct MountFlags {
    pub bits: AtomicU32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MountOptions {
    _private: (),
}

pub enum SourceLabel {
    BlockPath,
    Magic,
    None,
}

static MOUNT_IDENTITY_ZONE: Zone<MountIdentity> = Zone::const_new();
static MOUNT_NAMESPACE_ZONE: Zone<MountNamespace> = Zone::const_new();
static MOUNT_PAYLOAD_ZONE: Zone<MountPayload> = Zone::const_new();

unsafe impl ZoneAllocated for MountIdentity {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for MountNamespace {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_NAMESPACE_ZONE
    }
}

unsafe impl ZoneAllocated for MountPayload {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_PAYLOAD_ZONE
    }
}

impl MountPayloadBinding {
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    pub(crate) fn attached_cap(&self) -> Option<Cap<MountPayload>> {
        match self {
            Self::Attached(payload) => Some(payload.clone().into_cap()),
            Self::Pending | Self::Detached => None,
        }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl MountParentSlot {
    pub(crate) fn new_for_test() -> Self {
        Self { _private: () }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl MountChildren {
    pub(crate) fn new_for_test() -> Self {
        Self { _private: () }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl MountChildLink {
    pub(crate) fn new_for_test() -> Self {
        Self { _private: () }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl AllMounts {
    pub(crate) fn new_for_test() -> Self {
        Self { _private: () }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl MountOptions {
    pub(crate) fn new_for_test() -> Self {
        Self { _private: () }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
impl MountpointIndex {
    pub(crate) fn empty_for_test() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
        }
    }
}

/// Test-only helpers for constructing mount zone fixtures.
#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) mod testing {
    use super::*;
    use crate::vfs::structure::DEntry;
    #[cfg(test)]
    use crate::vfs::structure::{
        DEntryChildren, DEntryKey, FsObjectId, InodeMeta, NameOwned, RNode, RNodeBacking, RNodeKey,
        StructPayload,
    };
    use alloc::sync::Arc;
    use tx_substrate::zone;

    #[cfg(test)]
    struct DummyFs;

    #[cfg(test)]
    impl crate::vfs::fs_ops::FsOps for DummyFs {
        fn lookup<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<FsObjectId> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn load_inode_meta<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<InodeMeta> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn serialize_inode_meta<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn create_inode<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &crate::vfs::fs_ops::Credential,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<(FsObjectId, InodeMeta)> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn unlink<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn rename<'g>(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn link<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn mkdir<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &crate::vfs::fs_ops::Credential,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<(FsObjectId, InodeMeta)> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn rmdir<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn symlink<'g>(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &crate::vfs::fs_ops::Credential,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<(FsObjectId, InodeMeta)> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn readdir<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: crate::vfs::fs_ops::DirCursor,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<
            Option<(crate::vfs::fs_ops::DirEntry, crate::vfs::fs_ops::DirCursor)>,
        > {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn destroy_inode<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }
    }

    #[cfg(test)]
    impl crate::page_backed::FsPageBacking for DummyFs {
        fn fetch_page<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<crate::page_backed::Frame> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn flush_page<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &crate::page_backed::Frame,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn truncate<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }

        fn fsync<'g>(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &'g Guard<'g>,
        ) -> crate::step::StepOutcome<()> {
            crate::step::StepOutcome::Err(crate::step::Errno::NotImplemented)
        }
    }

    pub(crate) fn make_payload_with_backing_for_test(
        fs_ops: Arc<dyn crate::vfs::fs_ops::FsOps>,
        fs_page_backing: Arc<dyn crate::page_backed::FsPageBacking>,
    ) -> Cap<MountPayload> {
        let reservation = zone::reserve_for::<MountPayload>().expect("MountPayload reservation");
        zone::sign_for(
            reservation,
            MountPayload {
                payload_pin_count: AtomicU32::new(0),
                fs_ops,
                fs_page_backing,
                backing: None,
                dev_id: DevId(1),
                options: MountOptions::new_for_test(),
                fstype: "dummy",
                source_label: SourceLabel::None,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn make_payload_with_fs_ops_for_test(
        fs_ops: Arc<dyn crate::vfs::fs_ops::FsOps>,
    ) -> Cap<MountPayload> {
        make_payload_with_backing_for_test(fs_ops, Arc::new(DummyFs))
    }

    #[cfg(test)]
    pub(crate) fn make_payload_for_test() -> Cap<MountPayload> {
        make_payload_with_fs_ops_for_test(Arc::new(DummyFs))
    }

    #[cfg(test)]
    pub(crate) fn make_bootstrap_pair_with_payload_for_test(
        root: Cap<DEntry>,
    ) -> (Cap<MountIdentity>, Cap<MountNamespace>, Cap<MountPayload>) {
        let payload = make_payload_for_test();
        let (mi_cap, ns_cap) = make_bootstrap_pair_with_payload_binding_for_test(
            root,
            MountPayloadBinding::Attached(PayloadCap::from_cap(payload.clone())),
        );
        (mi_cap, ns_cap, payload)
    }

    #[cfg(test)]
    pub(crate) fn make_bootstrap_pair_with_fs_ops_for_test(
        root: Cap<DEntry>,
        fs_ops: Arc<dyn crate::vfs::fs_ops::FsOps>,
    ) -> (Cap<MountIdentity>, Cap<MountNamespace>, Cap<MountPayload>) {
        let payload = make_payload_with_fs_ops_for_test(fs_ops);
        let (mi_cap, ns_cap) = make_bootstrap_pair_with_payload_binding_for_test(
            root,
            MountPayloadBinding::Attached(PayloadCap::from_cap(payload.clone())),
        );
        (mi_cap, ns_cap, payload)
    }

    pub(crate) fn make_bootstrap_pair_with_backend_for_test(
        root: Cap<DEntry>,
        fs_ops: Arc<dyn crate::vfs::fs_ops::FsOps>,
        fs_page_backing: Arc<dyn crate::page_backed::FsPageBacking>,
    ) -> (Cap<MountIdentity>, Cap<MountNamespace>, Cap<MountPayload>) {
        let payload = make_payload_with_backing_for_test(fs_ops, fs_page_backing);
        let (mi_cap, ns_cap) = make_bootstrap_pair_with_payload_binding_for_test(
            root,
            MountPayloadBinding::Attached(PayloadCap::from_cap(payload.clone())),
        );
        (mi_cap, ns_cap, payload)
    }

    /// Construct a (Cap<MountIdentity>, Cap<MountNamespace>) pair.
    ///
    /// The two types mutually reference each other. `is_mount_root` is a stub
    /// that returns `false` without dereferencing `mnt_ns` or `root_mount`, so
    /// the warm-path tests are safe even though the cycle is broken only via
    /// `peek_cap` (the slot is reserved but not yet live when the Cap is created;
    /// it becomes live after the corresponding `sign_for` call).
    ///
    /// # Safety
    ///
    /// Neither of the returned caps may be dereferenced inside a field that forms
    /// the cycle (`MountNamespace::root_mount`, `MountIdentity::mnt_ns`) until
    /// after this function returns.
    #[cfg(test)]
    pub(crate) fn make_bootstrap_pair_for_test(
        root: Cap<DEntry>,
    ) -> (Cap<MountIdentity>, Cap<MountNamespace>) {
        make_bootstrap_pair_with_payload_binding_for_test(root, MountPayloadBinding::Pending)
    }

    fn make_bootstrap_pair_with_payload_binding_for_test(
        root: Cap<DEntry>,
        payload: MountPayloadBinding,
    ) -> (Cap<MountIdentity>, Cap<MountNamespace>) {
        let mi_res = zone::reserve_for::<MountIdentity>().expect("MountIdentity reservation");
        let ns_res = zone::reserve_for::<MountNamespace>().expect("MountNamespace reservation");

        // Safety: peek_reservation_cap returns a Cap pointing at the reserved-but-not-yet-live
        // slot. We use it only to fill the circular reference fields; neither
        // is_mount_root nor any other warm-path code dereferences these fields.
        let mi_cap_preview = unsafe { tx_substrate::zone::testing::peek_reservation_cap(&mi_res) };
        let ns_cap_preview = unsafe { tx_substrate::zone::testing::peek_reservation_cap(&ns_res) };

        let ns_cap = zone::sign_for(
            ns_res,
            MountNamespace {
                root_mount: mi_cap_preview,
                mountpoint_index: MountpointIndex::empty_for_test(),
                all_mounts: AllMounts::new_for_test(),
            },
        );

        let mi_cap = zone::sign_for(
            mi_res,
            MountIdentity {
                mountpoint: root.clone(),
                root_dentry: root,
                parent: MountParentSlot::new_for_test(),
                mnt_ns: MountNamespaceBinding {
                    namespace: ns_cap_preview,
                },
                children: MountChildren::new_for_test(),
                child_chain: MountChildLink::new_for_test(),
                payload,
                flags: MountFlags {
                    bits: AtomicU32::new(0),
                },
                propagation: AtomicU8::new(0),
                umount_port: UmountPort {
                    port: RawPort::new(),
                },
            },
        );

        (mi_cap, ns_cap)
    }

    #[cfg(test)]
    pub(crate) fn make_rnode_for_test(key: u64, mode: u16) -> Cap<RNode> {
        let res = zone::reserve_for::<RNode>().expect("RNode reservation");
        zone::sign_for(
            res,
            RNode {
                key: RNodeKey(key),
                fs_object_id: FsObjectId(key),
                meta: InodeMeta {
                    mode,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    atime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                    mtime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                    ctime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                    nlinks: 1,
                    blocks: 0,
                    flags: 0,
                },
                backing: RNodeBacking::StructBacked {
                    payload: StructPayload::Deferred,
                },
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn make_dentry_for_test(key: u64, name: &[u8], rnode: Cap<RNode>) -> Cap<DEntry> {
        let res = zone::reserve_for::<DEntry>().expect("DEntry reservation");
        zone::sign_for(
            res,
            DEntry {
                key: DEntryKey(key),
                name: NameOwned::from_component(name).expect("valid name"),
                rnode,
                children: DEntryChildren::new(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::testing::make_payload_for_test;
    use super::MountPayloadPin;
    use tx_substrate::zone;

    fn setup() {
        tx_substrate::testing::init_host_for_test_once();
        let _ = zone::register_zone_for::<crate::mount::structure::MountPayload>();
    }

    #[test]
    fn mount_payload_pin_acquire_and_drop_tracks_count() {
        let _serial = crate::test_support::EpochTestGuard::acquire();
        setup();
        let payload = make_payload_for_test();

        assert_eq!(payload.payload_pin_count.load(Ordering::Acquire), 0);

        {
            let _pin = MountPayloadPin::acquire(&payload);
            assert_eq!(payload.payload_pin_count.load(Ordering::Acquire), 1);
        }

        assert_eq!(payload.payload_pin_count.load(Ordering::Acquire), 0);
    }
}
