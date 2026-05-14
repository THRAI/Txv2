//! Mount identity, payload, and backend bootstrap shells.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub mod adapter;

use adapter::runtime::{
    self, Cap, Dead, Entity, IdentitySlot, PayloadBinding, PayloadCap, PayloadPolicy, SpinMutex,
    Zone, ZoneAllocated, ZoneError,
};

use crate::device::BlockDevice;
use crate::execution::KernelResult;
use crate::page_backed::{FsPageBacking, PageContainer};
use crate::vfs::{DEntry, FsObjectId, FsOps, InodeMeta, RNode};

static MOUNT_IDENTITY_ZONE: Zone<MountIdentity> = Zone::const_new();
static MOUNT_PAYLOAD_ZONE: Zone<MountPayload> = Zone::const_new();
static MOUNT_NAMESPACE_ZONE: Zone<MountNamespace> = Zone::const_new();

unsafe impl ZoneAllocated for MountIdentity {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for MountPayload {
    type Policy = PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &MOUNT_PAYLOAD_ZONE
    }
}

unsafe impl ZoneAllocated for MountNamespace {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_NAMESPACE_ZONE
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MountId(u64);

impl MountId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DevId(u32);

impl DevId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountFlags(u64);

impl MountFlags {
    pub const READ_ONLY: Self = Self(1 << 0);
    pub const NO_ATIME: Self = Self(1 << 1);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountOptions {
    pub flags: MountFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceLabel {
    Static(&'static str),
    Anonymous,
}

pub struct MountPayload {
    payload_pin_count: AtomicU32,
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub backing: Option<Arc<dyn BlockDevice>>,
    pub dev_id: DevId,
    pub options: MountOptions,
    pub fstype: &'static str,
    pub source_label: SourceLabel,
}

impl MountPayload {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
    ) -> Self {
        Self {
            payload_pin_count: AtomicU32::new(0),
            fs_ops,
            fs_page_backing,
            backing,
            dev_id,
            options,
            fstype,
            source_label,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_cap(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
    ) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new(
            fs_ops,
            fs_page_backing,
            backing,
            dev_id,
            options,
            fstype,
            source_label,
        ))
    }

    pub fn payload_pin_count(&self) -> u32 {
        self.payload_pin_count.load(Ordering::Acquire)
    }

    pub fn fs_ops(&self) -> &Arc<dyn FsOps> {
        &self.fs_ops
    }

    pub fn fs_page_backing(&self) -> &Arc<dyn FsPageBacking> {
        &self.fs_page_backing
    }
}

impl core::fmt::Debug for MountPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountPayload")
            .field("payload_pin_count", &self.payload_pin_count())
            .field("dev_id", &self.dev_id)
            .field("options", &self.options)
            .field("fstype", &self.fstype)
            .field("source_label", &self.source_label)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct MountPayloadPin {
    payload: Cap<MountPayload>,
}

impl MountPayloadPin {
    pub fn acquire(payload: &PayloadCap<MountPayload>) -> Self {
        payload.payload_pin_count.fetch_add(1, Ordering::AcqRel);
        Self {
            payload: payload.clone().into_cap(),
        }
    }

    pub fn payload(&self) -> &Cap<MountPayload> {
        &self.payload
    }
}

impl Clone for MountPayloadPin {
    fn clone(&self) -> Self {
        Self::acquire(&PayloadCap::from_cap(self.payload.clone()))
    }
}

impl PartialEq for MountPayloadPin {
    fn eq(&self, other: &Self) -> bool {
        self.payload.key() == other.payload.key()
    }
}

impl Eq for MountPayloadPin {}

impl Drop for MountPayloadPin {
    fn drop(&mut self) {
        self.payload
            .payload_pin_count
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct MountIdentity {
    id: MountId,
    mountpoint: Option<Cap<DEntry>>,
    root: Cap<RNode>,
    parent: Option<Cap<MountIdentity>>,
    payload: PayloadBinding<MountPayload>,
    flags: MountFlags,
}

impl MountIdentity {
    pub fn new(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root: Cap<RNode>,
        parent: Option<Cap<MountIdentity>>,
        payload: PayloadBinding<MountPayload>,
        flags: MountFlags,
    ) -> Self {
        Self {
            id,
            mountpoint,
            root,
            parent,
            payload,
            flags,
        }
    }

    pub fn new_cap(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root: Cap<RNode>,
        parent: Option<Cap<MountIdentity>>,
        payload: Cap<MountPayload>,
        flags: MountFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        let payload = PayloadBinding::installed(PayloadCap::from_cap(payload));
        runtime::sign(Self::new(id, mountpoint, root, parent, payload, flags))
    }

    pub const fn id(&self) -> MountId {
        self.id
    }

    pub fn mountpoint(&self) -> Option<&Cap<DEntry>> {
        self.mountpoint.as_ref()
    }

    pub fn root(&self) -> &Cap<RNode> {
        &self.root
    }

    pub fn parent(&self) -> Option<&Cap<MountIdentity>> {
        self.parent.as_ref()
    }

    pub fn payload_binding(&self) -> &PayloadBinding<MountPayload> {
        &self.payload
    }

    pub fn payload_cap(&self) -> Result<PayloadCap<MountPayload>, Dead> {
        self.payload.upgrade()
    }

    pub const fn flags(&self) -> MountFlags {
        self.flags
    }
}

impl Entity for MountIdentity {
    type OperationalEvidence = MountPayloadPin;

    fn upgrade_operational(identity: &Cap<Self>) -> Result<Self::OperationalEvidence, Dead> {
        Ok(MountPayloadPin::acquire(&identity.payload_cap()?))
    }
}

#[derive(Debug)]
pub struct MountNamespace {
    root: Cap<MountIdentity>,
}

impl MountNamespace {
    pub fn new(root: Cap<MountIdentity>) -> Self {
        Self { root }
    }

    pub fn new_cap(root: Cap<MountIdentity>) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new(root))
    }

    pub fn root(&self) -> &Cap<MountIdentity> {
        &self.root
    }
}

pub trait MetadataPcFactory: Send + Sync + 'static {
    fn create_metadata_pc(
        &self,
        start_block: u64,
        block_count: u64,
    ) -> KernelResult<Cap<PageContainer>>;
}

pub struct MountInitContext {
    pub block_device: Arc<dyn BlockDevice>,
    pub mount_id: MountId,
    pub metadata_pc_factory: Arc<dyn MetadataPcFactory>,
    pub options: MountOptions,
}

pub struct MountOutput {
    pub fs_ops: Arc<dyn crate::vfs::FsOps>,
    pub fs_page_backing: Arc<dyn crate::page_backed::FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

// === mount-point registry ============================================
//
// Day-1 dentry materialisation is fresh-on-each-lookup: the walker
// materialises a new `Cap<DEntry>` for every interior component
// rather than consulting a dentry cache. Mount hints stored on
// individual dentries (`DEntry::mounted`) therefore survive only as
// long as the dentry that carries them — which is "as long as a
// downstream caller holds a strong cap to the mountpoint dentry".
// Walks through `step_walk` *don't* hold one (the walker holds
// caps to the chain of dentries it materialised, none of which
// inherit the mountpoint dentry's hint).
//
// To bridge that gap without growing a full dentry cache, the
// kernel maintains a small mount-point table keyed by
// `(parent_mount_payload_ptr, child_fs_object_id)`: when the walker
// materialises a child dentry whose `(mount_payload_ptr,
// fs_object_id)` pair matches a registered entry, the walker
// upgrades into the registered `MountIdentity` and crosses the
// boundary. Init's `mount_devfs_at_dev` (and any future mount
// publication) registers an entry; tests can populate the table
// directly via [`register_mount`].
//
// Concurrency: a single `SpinMutex<Vec<...>>` is sufficient for the
// boot path's mount count (a handful) and matches the trio's
// existing `BootSpinMutex`-protected slots in `init.rs`. A future
// hot-path RCU dance is a follow-up if mount churn becomes a
// concern.

/// Registered mount entry. Keyed by the `(parent_mount_payload_ptr,
/// child_fs_object_id)` pair that uniquely identifies the
/// mount-point dentry on its parent filesystem. The
/// `mount_payload_ptr` is the raw pointer of the parent mount's
/// `MountPayload` `Cap`'s underlying allocation — sound to use as a
/// stable id because mount payloads are zone-allocated and never
/// reused while live caps exist.
struct MountTableEntry {
    parent_payload_ptr: usize,
    child_fs_object_id: FsObjectId,
    mount: IdentitySlot<MountIdentity>,
}

static MOUNT_TABLE: SpinMutex<Vec<MountTableEntry>> = SpinMutex::new(Vec::new());

/// Register a mount-point in the kernel's mount table.
///
/// `parent_payload` is the `Cap<MountPayload>` of the parent
/// filesystem (the one the mount-point dentry lives on);
/// `mountpoint_fs_object_id` is the inode id of that dentry on the
/// parent. `mount` is the `Cap<MountIdentity>` of the child mount
/// being registered.
///
/// The walker (`crate::vfs::walker::step_walk`) consults this table
/// when materialising a child dentry: a hit causes the walker to
/// upgrade the registered mount and continue from the mount's root
/// rnode + a fresh DEntry for it. Cite
/// `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1` for the publication
/// ordering: register *after* the mount's payload is signed and
/// before any walk-time observation could miss it.
pub fn register_mount(
    parent_payload: &Cap<MountPayload>,
    mountpoint_fs_object_id: FsObjectId,
    mount: Cap<MountIdentity>,
) {
    let parent_payload_ptr = cap_payload_ptr(parent_payload);
    let mut table = MOUNT_TABLE.lock();
    // Idempotent on identical parent + child_fs_object_id: if a
    // pre-existing entry matches, replace it with the new mount.
    // This matches `register_console_alias`'s upsert shape.
    for entry in table.iter_mut() {
        if entry.parent_payload_ptr == parent_payload_ptr
            && entry.child_fs_object_id == mountpoint_fs_object_id
        {
            entry.mount = IdentitySlot::from_cap(mount);
            return;
        }
    }
    table.push(MountTableEntry {
        parent_payload_ptr,
        child_fs_object_id: mountpoint_fs_object_id,
        mount: IdentitySlot::from_cap(mount),
    });
}

/// Look up a registered mount by (parent_payload, child_fs_object_id).
///
/// Returns `None` if no mount is registered for the given pair.
/// Used by the VFS walker.
pub fn mount_for(
    parent_payload: &Cap<MountPayload>,
    child_fs_object_id: FsObjectId,
) -> Option<Cap<MountIdentity>> {
    let parent_payload_ptr = cap_payload_ptr(parent_payload);
    let table = MOUNT_TABLE.lock();
    for entry in table.iter() {
        if entry.parent_payload_ptr == parent_payload_ptr
            && entry.child_fs_object_id == child_fs_object_id
        {
            return Some(entry.mount.clone_cap());
        }
    }
    None
}

/// Reset the mount table. Test-only.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_mount_table_for_test() {
    MOUNT_TABLE.lock().clear();
}

// === MountId / DevId allocators ======================================
//
// Per `txdoc:MOUNT-MOUNTPAYLOAD-1`,
// `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`
// (`docs/design/05_filesystem/MOUNT_v1.md`): every mount commits a
// fresh id at publication time. Day-1 boot wiring used hardcoded
// values (`MountId(1)` / `MountId(2)` for rootfs/devfs); centralising
// the allocation here lets follow-up mounts (extra tmpfs, future
// procfs, devpts) participate without each call site picking its own
// constant.
//
// Bootstrap-stability: the allocators are deterministic from cold
// start. `allocate_mount_id()` returns 1 on its first call, 2 on its
// second, etc.; same shape for `allocate_dev_id()`. The trio's
// `boot_smoke_*` tests assert on the literal MountId(1) / MountId(2)
// boot values; preserving the deterministic-from-cold-start guarantee
// keeps those assertions valid without churn.
//
// The `0` value is reserved for "unset / sentinel" so a default-
// constructed id never aliases a real mount.

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_DEV_ID: AtomicU32 = AtomicU32::new(1);

/// Allocate a fresh `MountId`. Deterministic from cold start: the
/// first call returns `MountId(1)`, the second `MountId(2)`, etc.
/// Mirrors the `allocate_tid` shape in
/// `crate::thread_runtime::structure`.
pub fn allocate_mount_id() -> MountId {
    MountId(NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed))
}

/// Allocate a fresh `DevId`. Deterministic from cold start: the first
/// call returns `DevId(1)`, the second `DevId(2)`, etc.
pub fn allocate_dev_id() -> DevId {
    DevId(NEXT_DEV_ID.fetch_add(1, Ordering::Relaxed))
}

/// Reset the mount-id counter to its post-boot starting value (1).
/// Test-only.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_mount_id_counter_for_test() {
    NEXT_MOUNT_ID.store(1, Ordering::Relaxed);
}

/// Reset the dev-id counter to its post-boot starting value (1).
/// Test-only.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_dev_id_counter_for_test() {
    NEXT_DEV_ID.store(1, Ordering::Relaxed);
}

/// Reach inside a `Cap<MountPayload>` for its underlying allocation
/// pointer. Used as a stable identity for the mount-table key. The
/// pointer is sound to use as a key because zone allocations never
/// reuse a slot while any cap is live; two caps to the same payload
/// produce identical pointers.
fn cap_payload_ptr(cap: &Cap<MountPayload>) -> usize {
    // `Cap::raw_addr_for_eq` returns the underlying zone slot's
    // address. Use it for the pointer-equality key. (`Cap` doesn't
    // implement `Eq` by-pointer publicly; we go through the public
    // accessor via a private helper here so the conversion is
    // contained.)
    cap_raw_addr(cap)
}

#[inline(always)]
fn cap_raw_addr<T>(cap: &Cap<T>) -> usize {
    // The walker treats the pointer as an opaque id; the actual
    // value is meaningless beyond equality. `Cap`'s `Debug`
    // formats the pointer, so we leverage `as_ptr_for_eq` if
    // available; otherwise we fall back to `&*cap` deref'd.
    //
    // tx_substrate's `Cap<T>` implements `Deref<Target = T>` —
    // the deref target's address is stable per cap.
    (&**cap as *const T) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{Errno, Guard};
    use crate::page_backed::{Frame, PageContainerKind};
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, NoProgress, StepOutcome};
    use crate::vfs::{Credential, DirCursor, DirEntry, InodeKind, RNodeBacking};

    struct MockFs;

    impl FsOps for MockFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<FsObjectId, NoProgress> {
            if name == b"root" {
                StepOutcome::done(FsObjectId::ROOT)
            } else {
                StepOutcome::err(V3Errno::ENOENT)
            }
        }

        fn load_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<InodeMeta, NoProgress> {
            StepOutcome::done(InodeMeta::new(InodeKind::Directory, 0o040755))
        }

        fn serialize_inode_meta(
            &self,
            _fs_object_id: FsObjectId,
            _meta: &InodeMeta,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn readdir(
            &self,
            _fs_object_id: FsObjectId,
            _cursor: DirCursor,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
            StepOutcome::done(None)
        }

        fn destroy_inode(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl FsPageBacking for MockFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Frame, NoProgress> {
            StepOutcome::err(V3Errno::ENOSYS)
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(V3Errno::EROFS)
        }

        fn fsync(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    #[test]
    fn mount_payload_stores_backend_traits_and_pins_are_explicit() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(1),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");

        assert_eq!(payload.payload_pin_count(), 0);
        {
            let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
            assert_eq!(pin.payload().dev_id, DevId::new(1));
            assert_eq!(payload.payload_pin_count(), 1);
        }
        assert_eq!(payload.payload_pin_count(), 0);
    }

    #[test]
    fn file_page_container_kind_carries_mount_payload_and_object_id() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(2),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Anonymous,
        )
        .expect("mount payload");
        let kind = PageContainerKind::File {
            mount: MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone())),
            fs_object_id: FsObjectId::new(99),
        };

        assert!(matches!(
            kind,
            PageContainerKind::File {
                ref mount,
                fs_object_id
            } if mount.payload().key() == payload.key() && fs_object_id == FsObjectId::new(99)
        ));
    }

    #[test]
    fn mount_identity_payload_binding_upgrades_and_operational_pin_counts_separately() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");

        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(3),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");
        let root = RNode::new_cap(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::PageBacked {
                pc: PageContainer::new_cap(
                    PageContainerKind::Anon {
                        swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
                    },
                    1,
                )
                .expect("page container"),
            },
        )
        .expect("root rnode");
        let mount = MountIdentity::new_cap(
            MountId::new(7),
            None,
            root,
            None,
            payload.clone(),
            MountFlags::empty(),
        )
        .expect("mount identity");

        let bound = mount.payload_cap().expect("payload binding upgrade");
        assert_eq!(bound.key(), payload.key());
        assert_eq!(payload.payload_pin_count(), 0);

        {
            let pin = MountPayloadPin::acquire(&bound);
            assert_eq!(pin.payload().key(), payload.key());
            assert_eq!(payload.payload_pin_count(), 1);
        }

        assert_eq!(payload.payload_pin_count(), 0);
    }

    // -- step_v3 free-fn probes -------------------------------------------
    //
    // Mount has no production `step_*` fns; the only `StepOutcome`-shaped
    // surface is the `MockFs` test fixture's `FsOps`/`FsPageBacking`
    // trait impls. These standalone free fns mirror representative
    // MockFs methods through the step_v3 outcome shape so the helper
    // surface (`StepOutcome::done` / `err`) is exercised here. The
    // trait impls above stay untouched.
    //
    // We import step_v3 types via `crate::vfs::adapter::step_engine`
    // under aliases (`V3Errno`, `NoProgress`, `StepOutcome`) so the
    // surface keeps working alongside `crate::execution::{Errno,
    // Guard, StepOutcome}` which is also in scope.
    //
    // Coverage:
    // - `mockfs_lookup_v3` — `Done` (happy path) + `Err` (ENOENT).
    // - `mockfs_load_inode_meta_v3` — single `Done` outcome over a
    //   non-trivial payload (`InodeMeta`).
    // - `mockfs_fetch_page_v3` — single `Err(ENOSYS)` outcome routed
    //   through the `From<execution::Errno> for step_v3::Errno` bridge
    //   (`Errno::into()`); pins that conversion path.

    /// step_v3-shape sibling of [`MockFs::lookup`]. Returns
    /// `Done(FsObjectId::ROOT)` for `b"root"`, else `Err(ENOENT)`. The
    /// trait impl above is the only logic to mirror.
    fn mockfs_lookup_v3(
        _parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if name == b"root" {
            StepOutcome::done(FsObjectId::ROOT)
        } else {
            StepOutcome::err(V3Errno::ENOENT)
        }
    }

    /// step_v3-shape sibling of [`MockFs::load_inode_meta`]. Always
    /// returns `Done(InodeMeta::new(Directory, 0o040755))` (the same
    /// constant the trait impl returns). Pins the `done()` helper
    /// against a non-trivial payload type.
    fn mockfs_load_inode_meta_v3(
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        StepOutcome::done(InodeMeta::new(InodeKind::Directory, 0o040755))
    }

    /// step_v3-shape sibling of [`MockFs::fetch_page`]. Always returns
    /// `Err(ENOSYS)`, routed through the `From<execution::Errno> for
    /// step_v3::Errno` bridge so any drift in the errno catalog fails
    /// this test. Mirrors how a real mount-side step fn would surface
    /// an `execution::Errno` into a step_v3 outcome:
    /// `let errno: step_v3::Errno = exec_err.into()`.
    fn mockfs_fetch_page_v3(
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        let exec_err = Errno::ENOSYS;
        let v3_err: V3Errno = exec_err.into();
        StepOutcome::err(v3_err)
    }

    #[test]
    fn mockfs_lookup_v3_known_name_returns_done_root() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = crate::vfs::adapter::step_engine::guard();
        let outcome = mockfs_lookup_v3(FsObjectId::ROOT, b"root", &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(id) => {
                assert_eq!(id, FsObjectId::ROOT);
            }
            other => panic!("expected v3 Done(ROOT), got {other:?}"),
        }
    }

    #[test]
    fn mockfs_lookup_v3_unknown_name_returns_err_enoent() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = crate::vfs::adapter::step_engine::guard();
        let outcome = mockfs_lookup_v3(FsObjectId::ROOT, b"nope", &guard);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::ENOENT) => {}
            _ => panic!("expected v3 Err(ENOENT), got {outcome:?}"),
        }
    }

    #[test]
    fn mockfs_load_inode_meta_v3_returns_done_directory_meta() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = crate::vfs::adapter::step_engine::guard();
        let outcome = mockfs_load_inode_meta_v3(FsObjectId::ROOT, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Done(meta) => {
                assert_eq!(meta.kind(), InodeKind::Directory);
                assert_eq!(meta.mode, 0o040755);
            }
            other => panic!("expected v3 Done(InodeMeta), got {other:?}"),
        }
    }

    #[test]
    fn mockfs_fetch_page_v3_returns_err_enosys_via_v4_into_v3_bridge() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = crate::vfs::adapter::step_engine::guard();
        let outcome = mockfs_fetch_page_v3(FsObjectId::ROOT, 0, &guard);
        drop(guard);
        match outcome {
            StepOutcome::Err(V3Errno::ENOSYS) => {}
            _ => panic!("expected v3 Err(ENOSYS), got {outcome:?}"),
        }
    }
}
