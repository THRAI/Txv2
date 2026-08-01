//! Mount identity, payload, and backend bootstrap shells.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub mod adapter;
pub mod settlement;

pub use settlement::{MountSettlementOp, MountTransactionFrontier, SettlementScope};

use adapter::runtime::{
    self, Cap, Dead, Entity, IdentitySlot, PayloadBinding, PayloadCap, PayloadPolicy, SlotKey,
    SpinMutex, Zone, ZoneAllocated, ZoneError,
};

use crate::device::BlockDevice;
use crate::execution::Errno;
use crate::execution::KernelResult;
use crate::fs_iface::{
    BackendPageRequest, BackendPlan, BackendPlanResume, BackendPlanner, FsObjectKey, IoDataSource,
    IoDataTarget,
};
use crate::io_manager::page::{PageIoRequest, service::PageServiceBackendContext};
use crate::page_backed::{ErrorCursor, FsPageBacking, PageContainer};
use crate::vfs::{
    DEntry, FsObjectId, FsOps, InlineName, InodeMeta, RNode,
    adapter::step_engine::{Guard, NoProgress, StepOutcome},
    render_dentry_path,
};

static MOUNT_IDENTITY_ZONE: Zone<MountIdentity> = Zone::const_new();
static MOUNT_PAYLOAD_ZONE: Zone<MountPayload> = Zone::const_new();
static MOUNT_NAMESPACE_ZONE: Zone<MountNamespace> = Zone::const_new();
static MOUNT_API_FILE_ZONE: Zone<MountApiFile> = Zone::const_new();
static BACKGROUND_MOUNT_SETTLEMENT_QUEUE: SpinMutex<Vec<MountSettlementOp>> =
    SpinMutex::new(Vec::new());

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

unsafe impl ZoneAllocated for MountApiFile {
    fn zone() -> &'static Zone<Self> {
        &MOUNT_API_FILE_ZONE
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
    pub const NOSUID: Self = Self(1 << 2);
    pub const NODEV: Self = Self(1 << 3);
    pub const NOEXEC: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    pub const fn union(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
}

/// Mount propagation type (Linux shared-subtree semantics).
///
/// - `Private` (default): mounts/umounts under this mount do not propagate.
/// - `Shared`: this mount belongs to a peer group (`peer_group` id). A
///   mount or umount at a mountpoint within a shared mount's subtree is
///   replicated at the corresponding location under every peer.
/// - `Slave`: receives propagation from its master peer group but does not
///   send. (v1: recorded but treated like `Private` for send; receive is a
///   follow-up.)
/// - `Unbindable`: private and cannot be bind-mounted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Propagation {
    Private,
    Shared,
    Slave,
    Unbindable,
}

impl Propagation {
    pub const fn to_bits(self) -> u64 {
        match self {
            Propagation::Private => 0,
            Propagation::Shared => 1,
            Propagation::Slave => 2,
            Propagation::Unbindable => 3,
        }
    }

    pub const fn from_bits(bits: u64) -> Self {
        match bits {
            1 => Propagation::Shared,
            2 => Propagation::Slave,
            3 => Propagation::Unbindable,
            _ => Propagation::Private,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountApiFileKind {
    FsContext,
    DetachedMount,
    OpenTree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsContextMode {
    New,
    Reconfigure,
}

#[derive(Clone, Debug)]
pub struct DetachedMountState {
    pub payload: Cap<MountPayload>,
    pub root: Cap<RNode>,
    pub flags: MountFlags,
}

#[derive(Debug)]
pub struct MountApiFile {
    kind: MountApiFileKind,
    mode: Option<FsContextMode>,
    fstype: &'static str,
    source: SpinMutex<Option<Vec<u8>>>,
    mount_flags: AtomicU64,
    picked_mount: SpinMutex<Option<Cap<MountIdentity>>>,
    detached: SpinMutex<Option<DetachedMountState>>,
}

impl MountApiFile {
    pub fn new_fs_context(
        fstype: &'static str,
        mode: FsContextMode,
        picked_mount: Option<Cap<MountIdentity>>,
    ) -> Self {
        Self {
            kind: MountApiFileKind::FsContext,
            mode: Some(mode),
            fstype,
            source: SpinMutex::new(None),
            mount_flags: AtomicU64::new(0),
            picked_mount: SpinMutex::new(picked_mount),
            detached: SpinMutex::new(None),
        }
    }

    pub fn new_fs_context_cap(
        fstype: &'static str,
        mode: FsContextMode,
        picked_mount: Option<Cap<MountIdentity>>,
    ) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new_fs_context(fstype, mode, picked_mount))
    }

    pub fn new_detached_mount(
        kind: MountApiFileKind,
        payload: Cap<MountPayload>,
        root: Cap<RNode>,
        flags: MountFlags,
    ) -> Self {
        debug_assert!(matches!(
            kind,
            MountApiFileKind::DetachedMount | MountApiFileKind::OpenTree
        ));
        let fstype = payload.fstype;
        Self {
            kind,
            mode: None,
            fstype,
            source: SpinMutex::new(None),
            mount_flags: AtomicU64::new(flags.bits()),
            picked_mount: SpinMutex::new(None),
            detached: SpinMutex::new(Some(DetachedMountState {
                payload,
                root,
                flags,
            })),
        }
    }

    pub fn new_detached_mount_cap(
        kind: MountApiFileKind,
        payload: Cap<MountPayload>,
        root: Cap<RNode>,
        flags: MountFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new_detached_mount(kind, payload, root, flags))
    }

    pub const fn kind(&self) -> MountApiFileKind {
        self.kind
    }

    pub const fn mode(&self) -> Option<FsContextMode> {
        self.mode
    }

    pub const fn fstype(&self) -> &'static str {
        self.fstype
    }

    pub fn source(&self) -> Option<Vec<u8>> {
        self.source.lock().clone()
    }

    pub fn set_source(&self, source: Vec<u8>) {
        *self.source.lock() = Some(source);
    }

    pub fn mount_flags(&self) -> MountFlags {
        MountFlags::from_bits(self.mount_flags.load(Ordering::Acquire))
    }

    pub fn set_mount_flags(&self, flags: MountFlags) {
        self.mount_flags.store(flags.bits(), Ordering::Release);
    }

    pub fn picked_mount(&self) -> Option<Cap<MountIdentity>> {
        self.picked_mount.lock().clone()
    }

    pub fn detached(&self) -> Option<DetachedMountState> {
        self.detached.lock().clone()
    }

    pub fn replace_detached(&self, next: Option<DetachedMountState>) -> Option<DetachedMountState> {
        core::mem::replace(&mut *self.detached.lock(), next)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountOptions {
    /// Mount flags (read-only, nosuid, noexec, etc.)
    pub flags: MountFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceLabel {
    Static(&'static str),
    Anonymous,
}

pub struct MountPayload {
    payload_pin_count: AtomicU32,
    runtime_cell: SpinMutex<settlement::MountRuntimeCell>,
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
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
        Self::new_with_backend_planner(
            fs_ops,
            fs_page_backing,
            backing,
            dev_id,
            options,
            fstype,
            source_label,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_backend_planner(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
        backend_planner: Option<Arc<dyn BackendPlanner>>,
    ) -> Self {
        Self {
            payload_pin_count: AtomicU32::new(0),
            runtime_cell: SpinMutex::new(settlement::MountRuntimeCell::new()),
            fs_ops,
            fs_page_backing,
            backend_planner,
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

    #[allow(clippy::too_many_arguments)]
    pub fn new_cap_with_backend_planner(
        fs_ops: Arc<dyn FsOps>,
        fs_page_backing: Arc<dyn FsPageBacking>,
        backing: Option<Arc<dyn BlockDevice>>,
        dev_id: DevId,
        options: MountOptions,
        fstype: &'static str,
        source_label: SourceLabel,
        backend_planner: Option<Arc<dyn BackendPlanner>>,
    ) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new_with_backend_planner(
            fs_ops,
            fs_page_backing,
            backing,
            dev_id,
            options,
            fstype,
            source_label,
            backend_planner,
        ))
    }

    pub fn payload_pin_count(&self) -> u32 {
        self.payload_pin_count.load(Ordering::Acquire)
    }

    pub fn runtime_state(&self) -> settlement::MountRuntimeState {
        self.runtime_cell.lock().state()
    }

    pub fn begin_lazy_detach(&self) -> Result<bool, Errno> {
        self.runtime_cell.lock().begin_lazy_detach()
    }

    pub fn try_claim_settlement(&self, scope: settlement::SettlementScope) -> Result<(), Errno> {
        self.runtime_cell.lock().try_claim_settlement(scope)
    }

    pub fn complete_settlement(&self, result: Result<(), Errno>) {
        self.runtime_cell.lock().complete_settlement(result);
    }

    pub fn observe_mount_error(&self) -> Option<Errno> {
        self.runtime_cell.lock().observe_mount_error()
    }

    pub fn observe_payload_error(&self) -> Option<Errno> {
        self.runtime_cell.lock().observe_payload_error()
    }

    pub fn snapshot_error_cursor(&self) -> ErrorCursor {
        self.runtime_cell.lock().snapshot_error_cursor()
    }

    pub fn observe_mount_error_with_cursor(&self, cursor: &mut ErrorCursor) -> Option<Errno> {
        self.runtime_cell
            .lock()
            .observe_mount_error_with_cursor(cursor)
    }

    pub fn observe_payload_error_with_cursor(&self, cursor: &mut ErrorCursor) -> Option<Errno> {
        self.runtime_cell
            .lock()
            .observe_payload_error_with_cursor(cursor)
    }

    pub fn snapshot_transaction_frontier(&self) -> MountTransactionFrontier {
        self.fs_ops.snapshot_mount_transaction_frontier()
    }

    pub fn fs_ops(&self) -> &Arc<dyn FsOps> {
        &self.fs_ops
    }

    pub fn fs_page_backing(&self) -> &Arc<dyn FsPageBacking> {
        &self.fs_page_backing
    }

    pub fn backend_planner(&self) -> Option<&dyn BackendPlanner> {
        self.backend_planner.as_deref()
    }

    pub fn prepare_backend_page_request(
        &self,
        request: &BackendPageRequest,
        guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        self.backend_planner
            .as_deref()
            .map_or(Ok(()), |planner| planner.prepare_page_io(request, guard))
    }

    pub fn plan_backend_page_request(
        &self,
        object: FsObjectKey,
        request: PageIoRequest,
    ) -> Option<BackendPlan> {
        self.backend_planner.as_deref().map(|planner| {
            planner.plan_page_io(BackendPageRequest::from_page_io_request(object, request))
        })
    }

    pub fn plan_backend_page_request_with_source(
        &self,
        object: FsObjectKey,
        request: PageIoRequest,
        source: IoDataSource,
    ) -> Option<BackendPlan> {
        self.backend_planner.as_deref().map(|planner| {
            planner.plan_page_io(BackendPageRequest::from_page_io_request_with_source(
                object, request, source,
            ))
        })
    }

    pub fn plan_backend_page_request_with_source_and_target(
        &self,
        object: FsObjectKey,
        request: PageIoRequest,
        source: IoDataSource,
        target: IoDataTarget,
    ) -> Option<BackendPlan> {
        self.backend_planner.as_deref().map(|planner| {
            planner.plan_page_io(BackendPageRequest::new_with_source_and_target(
                object,
                request.id,
                request.range,
                request.op,
                request.flags,
                request.generation_hint,
                source,
                target,
            ))
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MountPayloadBackendContext<'a> {
    payload: &'a MountPayload,
    object: FsObjectKey,
}

impl<'a> MountPayloadBackendContext<'a> {
    pub const fn new(payload: &'a MountPayload, object: FsObjectKey) -> Self {
        Self { payload, object }
    }

    pub const fn payload(self) -> &'a MountPayload {
        self.payload
    }

    pub const fn object(self) -> FsObjectKey {
        self.object
    }
}

impl PageServiceBackendContext for MountPayloadBackendContext<'_> {
    fn plan_submission(&self, request: PageIoRequest) -> Option<BackendPlan> {
        self.payload.plan_backend_page_request(self.object, request)
    }

    fn prepare_submission_with_source_and_target(
        &self,
        request: &PageIoRequest,
        source: &IoDataSource,
        target: &IoDataTarget,
        guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        self.payload.prepare_backend_page_request(
            &BackendPageRequest::new_with_source_and_target(
                self.object,
                request.id,
                request.range,
                request.op,
                request.flags,
                request.generation_hint,
                source.clone(),
                target.clone(),
            ),
            guard,
        )
    }

    fn plan_submission_with_source(
        &self,
        request: PageIoRequest,
        source: IoDataSource,
    ) -> Option<BackendPlan> {
        self.payload
            .plan_backend_page_request_with_source(self.object, request, source)
    }

    fn plan_submission_with_source_and_target(
        &self,
        request: PageIoRequest,
        source: IoDataSource,
        target: IoDataTarget,
    ) -> Option<BackendPlan> {
        self.payload
            .plan_backend_page_request_with_source_and_target(self.object, request, source, target)
    }

    fn resume_submission(&self, resume: BackendPlanResume) -> Option<BackendPlan> {
        self.payload
            .backend_planner()
            .map(|planner| planner.resume_page_io(resume))
    }
}

fn queue_background_mount_settlement(payload: &Cap<MountPayload>) {
    let pin = MountPayloadPin::acquire_cap(payload);
    if let Ok(op) = MountSettlementOp::new(pin, SettlementScope::Detach) {
        BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().push(op);
    }
}

pub fn background_mount_settlement_queue_len() -> usize {
    BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().len()
}

pub fn drive_background_mount_settlement_once(guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
    let Some(mut op) = BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().pop() else {
        return StepOutcome::done(());
    };

    match op.drive(guard) {
        StepOutcome::Done(()) => StepOutcome::done(()),
        StepOutcome::Err(errno) if Errno::from(errno) == Errno::EAGAIN => {
            BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().push(op);
            StepOutcome::err(Errno::EAGAIN.into())
        }
        StepOutcome::Err(errno) => StepOutcome::err(errno),
        StepOutcome::Continue { progress } => {
            BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().push(op);
            StepOutcome::Continue { progress }
        }
        StepOutcome::Yield { progress, shape } => {
            BACKGROUND_MOUNT_SETTLEMENT_QUEUE.lock().push(op);
            StepOutcome::Yield { progress, shape }
        }
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
            .field("has_backend_planner", &self.backend_planner.is_some())
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
        payload.runtime_cell.lock().note_payload_pin_acquired();
        Self {
            payload: payload.clone().into_cap(),
        }
    }

    pub fn acquire_cap(payload: &Cap<MountPayload>) -> Self {
        Self::acquire(&PayloadCap::from_cap(payload.clone()))
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
        let should_queue = self.payload.runtime_cell.lock().note_payload_pin_released();
        if should_queue {
            queue_background_mount_settlement(&self.payload);
        }
    }
}

#[derive(Debug)]
pub struct MountIdentity {
    id: MountId,
    mountpoint: SpinMutex<Option<Cap<DEntry>>>,
    root_dentry: Cap<DEntry>,
    parent: Option<Cap<MountIdentity>>,
    payload: PayloadBinding<MountPayload>,
    flags: AtomicU64,
    /// Propagation type bits (`Propagation::to_bits`). Default `Private`.
    propagation: AtomicU64,
    /// Peer-group id for `Shared` mounts (0 = not in any group). Mounts
    /// sharing a non-zero `peer_group` are peers and propagate to each
    /// other. Allocated by `allocate_peer_group_id`.
    peer_group: AtomicU64,
}

impl MountIdentity {
    pub fn new(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root_dentry: Cap<DEntry>,
        parent: Option<Cap<MountIdentity>>,
        payload: PayloadBinding<MountPayload>,
        flags: MountFlags,
    ) -> Self {
        Self {
            id,
            mountpoint: SpinMutex::new(mountpoint),
            root_dentry,
            parent,
            payload,
            flags: AtomicU64::new(flags.bits()),
            propagation: AtomicU64::new(Propagation::Private.to_bits()),
            peer_group: AtomicU64::new(0),
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
        let root_dentry = DEntry::new_cap(InlineName::ROOT, root)?;
        Self::new_cap_with_root_dentry(id, mountpoint, root_dentry, parent, payload, flags)
    }

    pub fn new_cap_with_root_dentry(
        id: MountId,
        mountpoint: Option<Cap<DEntry>>,
        root_dentry: Cap<DEntry>,
        parent: Option<Cap<MountIdentity>>,
        payload: Cap<MountPayload>,
        flags: MountFlags,
    ) -> Result<Cap<Self>, ZoneError> {
        let payload = PayloadBinding::installed(PayloadCap::from_cap(payload));
        runtime::sign(Self::new(
            id,
            mountpoint,
            root_dentry,
            parent,
            payload,
            flags,
        ))
    }

    pub const fn id(&self) -> MountId {
        self.id
    }

    pub fn mountpoint(&self) -> Option<Cap<DEntry>> {
        self.mountpoint.lock().clone()
    }

    fn replace_mountpoint(&self, mountpoint: Cap<DEntry>) {
        *self.mountpoint.lock() = Some(mountpoint);
    }

    pub fn root(&self) -> &Cap<RNode> {
        self.root_dentry.rnode()
    }

    pub fn root_dentry(&self) -> &Cap<DEntry> {
        &self.root_dentry
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

    pub fn flags(&self) -> MountFlags {
        MountFlags(self.flags.load(Ordering::Acquire))
    }

    /// Atomically replace mount flags (for `MS_REMOUNT`).
    pub fn set_flags(&self, new_flags: MountFlags) {
        self.flags.store(new_flags.bits(), Ordering::Release);
    }

    /// Current propagation type.
    pub fn propagation(&self) -> Propagation {
        Propagation::from_bits(self.propagation.load(Ordering::Acquire))
    }

    /// Set the propagation type (`mount --make-{shared,private,slave,unbindable}`).
    pub fn set_propagation(&self, prop: Propagation) {
        self.propagation.store(prop.to_bits(), Ordering::Release);
    }

    /// Peer-group id (0 = none). Mounts with the same non-zero id are peers.
    pub fn peer_group(&self) -> u64 {
        self.peer_group.load(Ordering::Acquire)
    }

    /// Assign the peer-group id (used when joining/forming a shared group).
    pub fn set_peer_group(&self, id: u64) {
        self.peer_group.store(id, Ordering::Release);
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
    mounts: SpinMutex<Vec<NamespaceMountEntry>>,
}

#[derive(Debug)]
struct NamespaceMountEntry {
    mountpoint_key: SlotKey,
    mount: IdentitySlot<MountIdentity>,
}

struct CloneIdentitySnapshot {
    original_key: SlotKey,
    mountpoint: Option<Cap<DEntry>>,
    root_dentry: Cap<DEntry>,
    parent_key: Option<SlotKey>,
    payload: Cap<MountPayload>,
    flags: MountFlags,
    propagation: Propagation,
    peer_group: u64,
}

/// Extract the mount namespace carried by a `/proc/<pid>/ns/mnt` fd, used by
/// `setns(2)`. Mirror of `net::net_namespace_payload_from_file`.
pub fn mount_namespace_cap_from_file(
    file: &Cap<crate::vfs::OpenFile>,
) -> Option<Cap<MountNamespace>> {
    match file.rnode().backing() {
        crate::vfs::RNodeBacking::StructBacked {
            payload: crate::vfs::structure::StructPayload::MountNamespace { payload },
        } => Some(payload.clone()),
        _ => None,
    }
}

impl MountNamespace {
    pub fn new(root: Cap<MountIdentity>) -> Self {
        Self {
            root,
            mounts: SpinMutex::new(Vec::new()),
        }
    }

    pub fn new_cap(root: Cap<MountIdentity>) -> Result<Cap<Self>, ZoneError> {
        runtime::sign(Self::new(root))
    }

    pub fn root(&self) -> &Cap<MountIdentity> {
        &self.root
    }

    /// Clone the stable VFS identity for this namespace's root.
    ///
    /// `MountIdentity` retains the canonical root `DEntry`; namespace clones
    /// retain that same identity. Callers must not infer the process root by
    /// walking cwd parent hints.
    pub fn root_dentry(&self) -> Cap<DEntry> {
        self.root.root_dentry().clone()
    }

    pub fn register_mount(&self, mountpoint: &Cap<DEntry>, mount: Cap<MountIdentity>) {
        debug_assert_eq!(
            mount.mountpoint().map(|dentry| dentry.key()),
            Some(mountpoint.key()),
            "namespace mountpoint key must match MountIdentity.mountpoint"
        );
        self.mounts.lock().push(NamespaceMountEntry {
            mountpoint_key: mountpoint.key(),
            mount: IdentitySlot::from_cap(mount),
        });
    }

    pub fn mount_for(&self, mountpoint: &Cap<DEntry>) -> Option<Cap<MountIdentity>> {
        let key = mountpoint.key();
        for e in self.mounts.lock().iter().rev() {
            if e.mountpoint_key == key {
                return Some(e.mount.clone_cap());
            }
        }
        None
    }

    pub fn mount_containing_dentry(&self, dentry: &Cap<DEntry>) -> Option<Cap<MountIdentity>> {
        let mut root = dentry.clone();
        while let Some(parent) = root.parent_hint() {
            root = parent;
        }
        if root.key() == self.root.root_dentry().key() {
            return Some(self.root.clone());
        }
        self.mounts
            .lock()
            .iter()
            .rev()
            .find(|entry| entry.mount.root_dentry().key() == root.key())
            .map(|entry| entry.mount.clone_cap())
    }

    pub fn snapshot_mounts(&self) -> alloc::vec::Vec<MountSnapshot> {
        let _guard = crate::vfs::adapter::step_engine::guard();
        self.mounts
            .lock()
            .iter()
            .filter_map(|e| {
                let m = e.mount.clone_cap();
                let d = m.mountpoint()?;
                let path = render_dentry_path(&d).unwrap_or_else(|| b"/?".to_vec());
                let p = m.payload_cap().ok()?;
                Some(MountSnapshot {
                    source: p.source_label,
                    mountpoint_path: path,
                    fstype: p.fstype,
                    flags: p.options.flags,
                })
            })
            .collect()
    }

    /// Capture the root payload plus each visible mounted payload in this
    /// namespace. Mount stacks contribute only their top-most visible row.
    pub fn snapshot_payload_pins(&self) -> alloc::vec::Vec<MountPayloadPin> {
        let mut pins = Vec::new();
        let mut seen = Vec::new();

        if let Ok(root_payload) = self.root.payload_cap() {
            seen.push(root_payload.key());
            pins.push(MountPayloadPin::acquire(&root_payload));
        }

        for entry in self.mounts.lock().iter().rev() {
            let payload = match entry.mount.clone_cap().payload_cap() {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            if seen.iter().any(|key| *key == payload.key()) {
                continue;
            }
            seen.push(payload.key());
            pins.push(MountPayloadPin::acquire(&payload));
        }

        pins
    }

    pub fn umount(&self, target: &Cap<DEntry>) -> Result<Cap<MountIdentity>, Errno> {
        let target_key = target.key();
        let mut t = self.mounts.lock();
        let pos = t.iter().rposition(|e| {
            e.mountpoint_key == target_key || e.mount.root_dentry().key() == target_key
        });
        if let Some(i) = pos {
            Ok(t.remove(i).mount.clone_cap())
        } else {
            Err(Errno::EINVAL)
        }
    }

    pub fn clone_ns(&self) -> Result<Cap<Self>, ZoneError> {
        self.clone_ns_with_snapshot_hook(|| {})
    }

    fn clone_ns_with_snapshot_hook<F>(&self, after_snapshot: F) -> Result<Cap<Self>, ZoneError>
    where
        F: FnOnce(),
    {
        let (rows, originals, root_key) = {
            let mounts = self.mounts.lock();
            let mut rows = Vec::new();
            rows.try_reserve_exact(mounts.len())
                .map_err(|_| ZoneError::AllocationFailed)?;
            for entry in mounts.iter() {
                rows.push((entry.mountpoint_key, entry.mount.clone_cap().key()));
            }

            // Gather every identity reachable from the namespace rows,
            // including parent ancestors without their own mountpoint row.
            let mut identities = Vec::new();
            identities
                .try_reserve(rows.len() + 1)
                .map_err(|_| ZoneError::AllocationFailed)?;
            identities.push(self.root.clone());
            for entry in mounts.iter() {
                let mount = entry.mount.clone_cap();
                if !identities
                    .iter()
                    .any(|existing| existing.key() == mount.key())
                {
                    identities.push(mount);
                }
            }
            let mut cursor = 0;
            while cursor < identities.len() {
                if let Some(parent) = identities[cursor].parent().cloned() {
                    if !identities
                        .iter()
                        .any(|existing| existing.key() == parent.key())
                    {
                        identities
                            .try_reserve(1)
                            .map_err(|_| ZoneError::AllocationFailed)?;
                        identities.push(parent);
                    }
                }
                cursor += 1;
            }

            // Namespace mutation takes mounts before identity placement. Read
            // all clone facts under the same order so a concurrent move cannot
            // pair an old row key with a new mountpoint.
            let mut originals = Vec::new();
            originals
                .try_reserve_exact(identities.len())
                .map_err(|_| ZoneError::AllocationFailed)?;
            for identity in identities {
                let payload = identity
                    .payload_cap()
                    .map_err(|_| ZoneError::InvalidState)?
                    .into_cap();
                originals.push(CloneIdentitySnapshot {
                    original_key: identity.key(),
                    mountpoint: identity.mountpoint(),
                    root_dentry: identity.root_dentry().clone(),
                    parent_key: identity.parent().map(Cap::key),
                    payload,
                    flags: identity.flags(),
                    propagation: identity.propagation(),
                    peer_group: identity.peer_group(),
                });
            }
            (rows, originals, self.root.key())
        };
        after_snapshot();

        // Clone in parent-topological order. Identity placement is private to
        // the new namespace; root DEntries and payload/backing remain shared.
        let mut remapped: Vec<(SlotKey, Cap<MountIdentity>)> = Vec::new();
        remapped
            .try_reserve_exact(originals.len())
            .map_err(|_| ZoneError::AllocationFailed)?;
        while remapped.len() < originals.len() {
            let before = remapped.len();
            for original in &originals {
                if remapped
                    .iter()
                    .any(|(key, _)| *key == original.original_key)
                {
                    continue;
                }
                let parent = match original.parent_key {
                    Some(parent_key) => match remapped
                        .iter()
                        .find(|(key, _)| *key == parent_key)
                        .map(|(_, cloned)| cloned.clone())
                    {
                        Some(parent) => Some(parent),
                        None => continue,
                    },
                    None => None,
                };
                let cloned = MountIdentity::new_cap_with_root_dentry(
                    allocate_mount_id(),
                    original.mountpoint.clone(),
                    original.root_dentry.clone(),
                    parent,
                    original.payload.clone(),
                    original.flags,
                )?;
                cloned.set_propagation(original.propagation);
                cloned.set_peer_group(original.peer_group);
                remapped.push((original.original_key, cloned));
            }
            if remapped.len() == before {
                return Err(ZoneError::InvalidState);
            }
        }

        let cloned_root = remapped
            .iter()
            .find(|(key, _)| *key == root_key)
            .map(|(_, mount)| mount.clone())
            .ok_or(ZoneError::InvalidState)?;
        let mut cloned = Vec::new();
        cloned
            .try_reserve_exact(rows.len())
            .map_err(|_| ZoneError::AllocationFailed)?;
        for (mountpoint_key, original_key) in rows {
            let mount = remapped
                .iter()
                .find(|(key, _)| *key == original_key)
                .map(|(_, mount)| mount.clone())
                .ok_or(ZoneError::InvalidState)?;
            cloned.push(NamespaceMountEntry {
                mountpoint_key,
                mount: IdentitySlot::from_cap(mount),
            });
        }
        runtime::sign(Self {
            root: cloned_root,
            mounts: SpinMutex::new(cloned),
        })
    }

    pub fn dotdot_parent_for_mount_root(&self, root: &Cap<DEntry>) -> Option<Cap<DEntry>> {
        let root_key = root.key();
        for entry in self.mounts.lock().iter().rev() {
            let mount = entry.mount.clone_cap();
            if mount.root_dentry().key() == root_key {
                let mountpoint = mount.mountpoint()?;
                return Some(mountpoint.parent_hint().unwrap_or(mountpoint));
            }
        }
        None
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
#[derive(Debug)]
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
/// upgrade the registered mount and continue from the mount's stable
/// root DEntry. Cite
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
    // Stack semantics: always push, allowing multiple mounts on the same
    // `(parent_payload, inode)` mountpoint. fs_bind tests stack binds on
    // overlapping inodes (e.g. `mount --bind dir dir` then `mount --bind src
    // dir`); the old upsert overwrote the earlier mount, which lost it and made
    // umount return EINVAL. `mount_for` returns the top (newest) of the stack
    // and `umount` pops it, matching Linux LIFO mountpoint semantics.
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
    // Top of stack = newest mount at this mountpoint (LIFO): iterate in reverse.
    for entry in table.iter().rev() {
        if entry.parent_payload_ptr == parent_payload_ptr
            && entry.child_fs_object_id == child_fs_object_id
        {
            return Some(entry.mount.clone_cap());
        }
    }
    None
}

pub fn dotdot_parent_for_mount_root(root: &Cap<DEntry>) -> Option<Cap<DEntry>> {
    let root_key = root.key();
    for entry in MOUNT_TABLE.lock().iter().rev() {
        let mount = entry.mount.clone_cap();
        if mount.root_dentry().key() == root_key {
            let mountpoint = mount.mountpoint()?;
            return Some(mountpoint.parent_hint().unwrap_or(mountpoint));
        }
    }
    None
}

// ============================================================================
// Mount table snapshot (for /proc/mounts)
// ============================================================================

/// Snapshot of one mount table entry, suitable for `/proc/mounts` rendering.
pub struct MountSnapshot {
    /// Device or source label (e.g. "rootfs", "/dev/vda").
    pub source: SourceLabel,
    /// Rendered mount-point path (e.g. "/", "/dev").
    pub mountpoint_path: alloc::vec::Vec<u8>,
    /// Filesystem type string (e.g. "tmpfs", "ext4").
    pub fstype: &'static str,
    /// Mount flags (read-only, nosuid, etc.)
    pub flags: MountFlags,
}

// ============================================================================
// Bind mount
// ============================================================================

/// Outcome of a bind-mount operation.
pub struct BindMountOutput {
    /// The newly-created mount identity.
    pub mount: Cap<MountIdentity>,
}

fn create_bind_mount_identity(
    source_dentry: &Cap<DEntry>,
    target_dentry: Cap<DEntry>,
    guard: &Guard<'_>,
) -> Result<Cap<MountIdentity>, crate::execution::Errno> {
    use crate::execution::Errno;

    let source_payload =
        crate::vfs::walker::mount_payload_for(source_dentry, guard).ok_or(Errno::ENODEV)?;
    MountIdentity::new_cap(
        allocate_mount_id(),
        Some(target_dentry),
        source_dentry.rnode().clone(),
        None,
        source_payload,
        MountFlags::empty(),
    )
    .map_err(|_| Errno::ENOMEM)
}

/// Create a bind mount: expose `source` at `target` path.
///
/// The bind mount shares the source filesystem's backend (`FsOps` +
/// `FsPageBacking`) — no new backend is created.  The walker sees
/// `target` as a mountpoint and traverses into `source`'s subtree.
///
/// v1: non-recursive, no propagation.
pub fn bind_mount(
    source_dentry: Cap<DEntry>,
    target_dentry: Cap<DEntry>,
    target_parent_payload: &Cap<MountPayload>,
    guard: &Guard<'_>,
) -> Result<BindMountOutput, crate::execution::Errno> {
    let target_fs_object_id = target_dentry.rnode().fs_object_id();
    let mount_cap = create_bind_mount_identity(&source_dentry, target_dentry, guard)?;

    register_mount(
        target_parent_payload,
        target_fs_object_id,
        mount_cap.clone(),
    );

    Ok(BindMountOutput { mount: mount_cap })
}

/// Bind-mount and publish into both the legacy global table and one explicit
/// mount namespace. Commit lock order is global table then namespace table;
/// both vectors reserve before either index is changed, so allocation failure
/// leaves no partial publication.
pub fn bind_mount_in_namespace(
    source_dentry: Cap<DEntry>,
    target_dentry: Cap<DEntry>,
    target_parent_payload: &Cap<MountPayload>,
    namespace: &Cap<MountNamespace>,
    guard: &Guard<'_>,
) -> Result<BindMountOutput, crate::execution::Errno> {
    use crate::execution::Errno;

    let target_fs_object_id = target_dentry.rnode().fs_object_id();
    let target_parent_ptr = cap_payload_ptr(target_parent_payload);
    let target_key = target_dentry.key();
    let mount_cap = create_bind_mount_identity(&source_dentry, target_dentry, guard)?;

    let mut global = MOUNT_TABLE.lock();
    let mut namespaced = namespace.mounts.lock();
    global.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
    namespaced.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
    global.push(MountTableEntry {
        parent_payload_ptr: target_parent_ptr,
        child_fs_object_id: target_fs_object_id,
        mount: IdentitySlot::from_cap(mount_cap.clone()),
    });
    namespaced.push(NamespaceMountEntry {
        mountpoint_key: target_key,
        mount: IdentitySlot::from_cap(mount_cap.clone()),
    });

    Ok(BindMountOutput { mount: mount_cap })
}

/// Relocate an existing mount (`mount --move source target`, MS_MOVE).
///
/// `source_dentry` is the *resolved* source (walker already crossed the
/// mount, so it's the moved subtree's stable root DEntry). Re-key the same subtree
/// under `target` and drop the source registration in one critical section.
pub fn move_mount(
    source_dentry: &Cap<DEntry>,
    target_dentry: Cap<DEntry>,
    target_parent_payload: &Cap<MountPayload>,
    _guard: &Guard<'_>,
) -> Result<(), crate::execution::Errno> {
    use crate::execution::Errno;

    let source_root_key = source_dentry.key();
    let target_fs_object_id = target_dentry.rnode().fs_object_id();
    let target_parent_ptr = cap_payload_ptr(target_parent_payload);

    let mut table = MOUNT_TABLE.lock();
    let src_idx = table
        .iter()
        .rposition(|entry| entry.mount.root_dentry().key() == source_root_key)
        .ok_or(Errno::EINVAL)?;
    let mount_cap = table[src_idx].mount.clone_cap();
    let mut moved = table.remove(src_idx);
    moved.parent_payload_ptr = target_parent_ptr;
    moved.child_fs_object_id = target_fs_object_id;
    mount_cap.replace_mountpoint(target_dentry);
    table.push(moved);
    Ok(())
}

/// Move one mounted identity between mountpoints while preserving its slot,
/// root projection, parent, flags, propagation, peer group, and payload.
///
/// The linearization lock order is global index -> namespace index -> identity
/// mountpoint slot. The namespace row selects the identity. A matching global
/// row is migrated only when it carries that same identity; cloned namespaces
/// intentionally have no legacy-global rows. All validation precedes mutation,
/// and the commit uses only remove, scalar replacement, and push operations
/// into vectors whose capacity was freed by the removals, so there is no
/// fallible step and no rollback window.
pub fn move_mount_in_namespace(
    source_dentry: &Cap<DEntry>,
    target_dentry: Cap<DEntry>,
    target_parent_payload: &Cap<MountPayload>,
    namespace: &Cap<MountNamespace>,
    _guard: &Guard<'_>,
) -> Result<(), crate::execution::Errno> {
    use crate::execution::Errno;

    let source_root_key = source_dentry.key();
    let target_fs_object_id = target_dentry.rnode().fs_object_id();
    let target_parent_ptr = cap_payload_ptr(target_parent_payload);
    let target_key = target_dentry.key();

    let mut global = MOUNT_TABLE.lock();
    let mut namespaced = namespace.mounts.lock();
    let namespace_idx = namespaced
        .iter()
        .rposition(|entry| entry.mount.root_dentry().key() == source_root_key)
        .ok_or(Errno::EINVAL)?;
    let mount_cap = namespaced[namespace_idx].mount.clone_cap();
    let global_idx = global
        .iter()
        .rposition(|entry| entry.mount.clone_cap().key() == mount_cap.key());

    let mut namespace_row = namespaced.remove(namespace_idx);
    namespace_row.mountpoint_key = target_key;
    mount_cap.replace_mountpoint(target_dentry);
    if let Some(global_idx) = global_idx {
        let mut global_row = global.remove(global_idx);
        global_row.parent_payload_ptr = target_parent_ptr;
        global_row.child_fs_object_id = target_fs_object_id;
        global.push(global_row);
    }
    namespaced.push(namespace_row);
    Ok(())
}

// ============================================================================
// Umount
// ============================================================================

/// Unmount a filesystem: remove the mount-point registration so
/// future walks no longer cross into this mount.
///
/// v1: synchronous detach only.  Returns `EINVAL` if the path is
/// not a registered mountpoint.
///
/// `umount` accepts either of two dentry shapes for `target_dentry`:
///
/// 1. The mountpoint dentry on the parent filesystem — matches the
///    registration key directly. Rare in practice since the walker
///    crosses mount boundaries.
/// 2. The mounted filesystem's root dentry — what the walker returns
///    when the user resolves the mount path. We detect this by
///    comparing every entry's `mount.root()` against
///    `target_dentry.rnode()`; on a hit we remove the entry without
///    requiring `parent_payload` to match.
pub fn umount(
    target_dentry: &Cap<DEntry>,
    parent_payload: &Cap<MountPayload>,
) -> Result<(), crate::execution::Errno> {
    use crate::execution::Errno;

    let parent_payload_ptr = cap_payload_ptr(parent_payload);
    let child_fs_object_id = target_dentry.rnode().fs_object_id();
    let target_key = target_dentry.key();

    let mut table = MOUNT_TABLE.lock();
    // Pop the top (newest) matching mount — `rposition` finds the LAST entry,
    // matching the LIFO stack semantics of `register_mount`/`mount_for`.
    // First try the registration key. If the user passed the
    // mountpoint dentry (matches the parent FS) the key is sound.
    let pos = table.iter().rposition(|entry| {
        entry.parent_payload_ptr == parent_payload_ptr
            && entry.child_fs_object_id == child_fs_object_id
    });
    // Fallback: the walker resolved the user path through the mount
    // and handed us the mounted FS's root dentry. Scan for an entry
    // whose registered mount has this rnode as its root.
    let pos = pos.or_else(|| {
        table
            .iter()
            .rposition(|entry| entry.mount.root_dentry().key() == target_key)
    });

    match pos {
        Some(idx) => {
            table.remove(idx);
            Ok(())
        }
        None => Err(Errno::EINVAL),
    }
}

/// Remove only the legacy-global row carrying this exact mount identity.
/// Cloned namespace identities have no global row, so their cleanup is a
/// successful no-op rather than a root-DEntry-key fallback into another
/// namespace's mount.
pub fn umount_identity_exact(mount: &Cap<MountIdentity>) -> bool {
    let mount_key = mount.key();
    let mut table = MOUNT_TABLE.lock();
    let Some(pos) = table
        .iter()
        .rposition(|entry| entry.mount.clone_cap().key() == mount_key)
    else {
        return false;
    };
    table.remove(pos);
    true
}

// ============================================================================
// Remount
// ============================================================================

/// Remount an existing mount with new flags (MS_REMOUNT).
///
/// `mount` is the `MountIdentity` cap obtained via
/// `mount_for()` or `snapshot_mounts()`.  Flags are atomically
/// replaced; concurrent walkers see either the old or new flags,
/// never a torn value.
///
/// v1: supports `MS_RDONLY` toggle; other flags accepted but no-op.
pub fn remount(mount: &Cap<MountIdentity>, new_flags: MountFlags) {
    mount.set_flags(new_flags);
}

// ============================================================================
// Filesystem factory
// ============================================================================

/// Output of a filesystem backend factory.
pub struct FsOutput {
    pub fs_ops: Arc<dyn crate::vfs::FsOps>,
    pub fs_page_backing: Arc<dyn crate::page_backed::FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
    pub fstype: &'static str,
}

/// Create a filesystem backend from a fstype string.
///
/// v1: returns `None` for all types — backend crates (tx-fs) are
/// not accessible from tx-subsystems.  The shims layer provides
/// the crate-level dispatch via `create_filesystem_for_mount`.
pub fn create_filesystem(_fstype: &str) -> Option<FsOutput> {
    None
}

/// Return a snapshot of all registered mounts.
///
/// Used by procfs to render `/proc/mounts`.  Each entry carries the
/// source label, mount-point path (computed via `render_dentry_path`),
/// filesystem type, and mount flags.
pub fn snapshot_mounts() -> alloc::vec::Vec<MountSnapshot> {
    let _guard = crate::vfs::adapter::step_engine::guard();
    let table = MOUNT_TABLE.lock();
    table
        .iter()
        .filter_map(|entry| {
            let mount = entry.mount.clone_cap();
            let dentry = mount.mountpoint()?;
            let path = render_dentry_path(&dentry).unwrap_or_else(|| b"/?".to_vec());
            let payload = mount.payload_cap().ok()?;
            Some(MountSnapshot {
                source: payload.source_label,
                mountpoint_path: path,
                fstype: payload.fstype,
                flags: payload.options.flags,
            })
        })
        .collect()
}

/// Reset the mount table. Test-only.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_mount_table_for_test() {
    MOUNT_TABLE.lock().clear();
}

/// Bootstrap a mount: create a `MountIdentity` for `source_payload`
/// on `mountpoint`, registered in the global mount table.
///
/// This is a **bootstrap helper** — it drives `FsOps` calls to
/// completion synchronously via an inline `drive()` loop, assuming
/// single-threaded boot context where no reactor is yet running.
/// Production hot-path mount/umount will be proper step ops.
///
/// Returns the new `MountIdentity` cap on success.
pub fn bootstrap_mount(
    source_payload: Cap<MountPayload>,
    mountpoint: Cap<DEntry>,
    parent_mount: Option<Cap<MountIdentity>>,
    guard: &Guard<'_>,
) -> Result<Cap<MountIdentity>, Errno> {
    let parent_payload = mountpoint
        .rnode()
        .containing_mount_weak()
        .and_then(|weak| weak.upgrade(guard))
        .ok_or(Errno::ENODEV)?;
    let fs_ops = source_payload.fs_ops().clone();
    let root_id = FsObjectId::ROOT;

    // Drive load_inode_meta to completion.
    let root_meta = drive_step_outcome_to_done(|| fs_ops.load_inode_meta(root_id, guard), guard)?;

    // Drive materialise_rnode to completion.
    let root_rnode = drive_step_outcome_to_done(
        || fs_ops.materialise_rnode(root_id, root_meta, &source_payload, guard),
        guard,
    )?;

    // Sign MountIdentity.
    let id = allocate_mount_id();
    let mount = MountIdentity::new_cap(
        id,
        Some(mountpoint.clone()),
        root_rnode,
        parent_mount,
        source_payload.clone(),
        MountFlags::empty(),
    )
    .map_err(|_| Errno::ENOMEM)?;

    // Register in the global mount table.
    let mountpoint_fs_object_id = mountpoint.rnode().fs_object_id();
    register_mount(&parent_payload, mountpoint_fs_object_id, mount.clone());

    Ok(mount)
}

/// Drive a `StepOutcome<T, NoProgress>` closure to completion,
/// spinning on `Yield` until `Done` or `Err`.
fn drive_step_outcome_to_done<T>(
    mut step_fn: impl FnMut() -> StepOutcome<T, NoProgress>,
    _guard: &Guard<'_>,
) -> Result<T, crate::execution::Errno> {
    loop {
        match step_fn() {
            StepOutcome::Done(value) => return Ok(value),
            StepOutcome::Err(e) => return Err(e.into()),
            _ => {
                // In bootstrap context, Continue/Yield are not expected;
                // spin once and retry.
                core::hint::spin_loop();
            }
        }
    }
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

static NEXT_PEER_GROUP_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh peer-group id for `mount --make-shared`. Always
/// non-zero (0 means "not in any peer group").
pub fn allocate_peer_group_id() -> u64 {
    NEXT_PEER_GROUP_ID.fetch_add(1, Ordering::Relaxed)
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
    use crate::fs_iface::{
        BackendPageRequest, BackendPlan, BackendPlanner, FsObjectKey, PageCompletion,
        PageCompletionList,
    };
    use crate::io_manager::block::BlockQueue;
    use crate::io_manager::page::{
        PageContainerKey, PageGeneration, PageIoCompletionKind, PageIoFlags, PageIoOp,
        PageIoPriority, PageIoRange, PageIoRequestId, PageIoResult,
        service::{
            PageService, PageServiceBackendSubmitOutcome, PageServiceDrivenWork, PageServiceDriver,
            PageServiceNext, PageServiceTurn, PageServiceWake, PageServiceWork,
        },
    };
    use crate::io_manager::runtime::{IoServiceKind, ServiceBudget, ServiceKick};
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

        fn materialise_rnode(
            &self,
            fs_object_id: FsObjectId,
            meta: InodeMeta,
            mount: &Cap<MountPayload>,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Cap<RNode>, NoProgress> {
            match RNode::new_cap_in_mount(fs_object_id, meta, RNodeBacking::Directory, mount) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(V3Errno::ENOMEM),
            }
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

        fn fsync_file(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    struct MockBackendPlanner;

    impl BackendPlanner for MockBackendPlanner {
        fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
            BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
                PageCompletion::new(
                    request.id,
                    request.range,
                    PageIoResult::Done,
                    request.generation_hint.unwrap_or(PageGeneration::new(0)),
                    PageIoCompletionKind::ReadInstalled,
                ),
            ]))
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
    fn mount_payload_hosts_optional_backend_planner_without_replacing_page_backing() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");

        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap_with_backend_planner(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(9),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
            Some(Arc::new(MockBackendPlanner) as Arc<dyn BackendPlanner>),
        )
        .expect("mount payload");

        assert!(Arc::strong_count(payload.fs_page_backing()) >= 1);
        let request = BackendPageRequest::new(
            FsObjectKey::new(123),
            PageIoRequestId::new(44),
            PageIoRange::new(7, 1),
            PageIoOp::Read,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(3)),
        );
        let plan = payload
            .backend_planner()
            .expect("mount-hosted backend planner")
            .plan_page_io(request);

        match plan {
            BackendPlan::Complete(completions) => {
                assert_eq!(completions.as_slice().len(), 1);
                assert_eq!(completions.as_slice()[0].id, PageIoRequestId::new(44));
                assert_eq!(completions.as_slice()[0].range, PageIoRange::new(7, 1));
            }
            other => panic!("expected completion plan, got {other:?}"),
        }
    }

    #[test]
    fn mount_payload_plans_page_service_submission_through_backend_planner() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");

        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap_with_backend_planner(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(11),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
            Some(Arc::new(MockBackendPlanner) as Arc<dyn BackendPlanner>),
        )
        .expect("mount payload");
        let mut service = PageService::new(4);
        let request_id = service
            .submit(
                PageContainerKey::new(88),
                PageIoRange::new(13, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(21)),
            )
            .expect("submit page request");
        let request = match service.drain_turn(crate::io_manager::runtime::ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Submission(request) => request,
                other => panic!("expected submission work, got {other:?}"),
            },
            other => panic!("expected work turn, got {other:?}"),
        };

        let plan = payload
            .plan_backend_page_request(FsObjectKey::new(500), request)
            .expect("mount-hosted planner");

        match plan {
            BackendPlan::Complete(completions) => {
                assert_eq!(completions.as_slice().len(), 1);
                assert_eq!(completions.as_slice()[0].id, request_id);
                assert_eq!(completions.as_slice()[0].range, PageIoRange::new(13, 1));
                assert_eq!(
                    completions.as_slice()[0].generation,
                    PageGeneration::new(21)
                );
            }
            other => panic!("expected completion plan, got {other:?}"),
        }
    }

    #[test]
    fn mount_payload_backend_context_drives_page_service_submission() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");

        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap_with_backend_planner(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(12),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
            Some(Arc::new(MockBackendPlanner) as Arc<dyn BackendPlanner>),
        )
        .expect("mount payload");
        let context = MountPayloadBackendContext::new(&payload, FsObjectKey::new(501));
        let mut service = PageService::new(4);
        let request_id = service
            .submit(
                PageContainerKey::new(89),
                PageIoRange::new(14, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(22)),
            )
            .expect("submit page request");
        let mut block_queue = BlockQueue::new(4);
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));
        let mut kicks = Vec::new();

        let driven =
            driver.drive_once_with_backend(&mut service, &context, &mut block_queue, |kick| {
                kicks.push(kick);
                true
            });

        assert_eq!(
            driven.work,
            alloc::vec![PageServiceDrivenWork::BackendSubmission(
                PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                    queued: 1,
                    wake: Some(PageServiceWake::Wake),
                }
            )]
        );
        assert_eq!(driven.next, PageServiceNext::Runnable);
        assert_eq!(kicks, alloc::vec![ServiceKick::new(IoServiceKind::Page)]);
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, request_id);
                    assert_eq!(route.completion.range, PageIoRange::new(14, 1));
                    assert_eq!(route.completion.generation, PageGeneration::new(22));
                }
                other => panic!("expected completion work, got {other:?}"),
            },
            other => panic!("expected queued completion turn, got {other:?}"),
        }
        assert!(block_queue.is_empty());
    }

    #[test]
    fn mount_payload_backend_context_returns_none_without_planner() {
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
            DevId::new(13),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");
        let context = MountPayloadBackendContext::new(&payload, FsObjectKey::new(502));
        let mut service = PageService::new(4);
        let request_id = service
            .submit(
                PageContainerKey::new(90),
                PageIoRange::new(15, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(23)),
            )
            .expect("submit page request");
        let mut block_queue = BlockQueue::new(4);
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));

        let driven =
            driver.drive_once_with_backend(&mut service, &context, &mut block_queue, |_| {
                panic!("unplanned mount payload request must not kick page service")
            });

        assert_eq!(
            driven.work,
            alloc::vec![PageServiceDrivenWork::UnplannedSubmission(
                crate::io_manager::page::PageIoRequest::new(
                    request_id,
                    PageContainerKey::new(90),
                    PageIoRange::new(15, 1),
                    PageIoOp::Read,
                    PageIoPriority::Demand,
                    PageIoFlags::DEMAND,
                    Some(PageGeneration::new(23)),
                )
            )]
        );
        assert_eq!(driven.next, PageServiceNext::Sleeping);
        assert!(block_queue.is_empty());
    }

    #[test]
    fn mount_payload_default_constructor_keeps_backend_planner_absent() {
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
            DevId::new(10),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("mock"),
        )
        .expect("mount payload");

        assert!(payload.backend_planner().is_none());
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

    #[test]
    fn mount_namespace_root_dentry_identity_is_stable_across_clone() {
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
            DevId::new(31),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("stable-root"),
        )
        .expect("mount payload");
        let root = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("root rnode");
        let mount = MountIdentity::new_cap(
            MountId::new(31),
            None,
            root,
            None,
            payload,
            MountFlags::empty(),
        )
        .expect("mount identity");
        let namespace = MountNamespace::new_cap(mount.clone()).expect("mount namespace");
        let cloned = namespace.clone_ns().expect("cloned namespace");

        assert_eq!(mount.root_dentry().key(), namespace.root_dentry().key());
        assert_eq!(namespace.root_dentry().key(), namespace.root_dentry().key());
        assert_eq!(namespace.root_dentry().key(), cloned.root_dentry().key());
    }

    #[test]
    fn snapshot_payload_pins_captures_root_and_visible_mount_payloads() {
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
            DevId::new(40),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("snapshot-root"),
        )
        .expect("root payload");
        let root_rnode = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("root rnode");
        let root_mount = MountIdentity::new_cap(
            MountId::new(40),
            None,
            root_rnode,
            None,
            payload.clone(),
            MountFlags::empty(),
        )
        .expect("root mount");
        let namespace = MountNamespace::new_cap(root_mount.clone()).expect("namespace");

        let mountpoint_rnode = RNode::new_cap_in_mount(
            FsObjectId::new(401),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("mountpoint rnode");
        let mountpoint = DEntry::new_cap(
            InlineName::new(b"snap").expect("mountpoint name"),
            mountpoint_rnode,
        )
        .expect("mountpoint");

        let child_fs = Arc::new(MockFs);
        let child_payload = MountPayload::new_cap(
            child_fs.clone() as Arc<dyn FsOps>,
            child_fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(41),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("snapshot-child"),
        )
        .expect("child payload");
        let child_root = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &child_payload,
        )
        .expect("child root");
        let child_mount = MountIdentity::new_cap(
            MountId::new(41),
            Some(mountpoint.clone()),
            child_root,
            None,
            child_payload.clone(),
            MountFlags::empty(),
        )
        .expect("child mount");
        namespace.register_mount(&mountpoint, child_mount.clone());

        assert_eq!(payload.payload_pin_count(), 0);
        assert_eq!(child_payload.payload_pin_count(), 0);

        let pins = namespace.snapshot_payload_pins();
        assert_eq!(pins.len(), 2);
        assert_eq!(payload.payload_pin_count(), 1);
        assert_eq!(child_payload.payload_pin_count(), 1);
        assert!(pins.iter().any(|pin| pin.payload().key() == payload.key()));
        assert!(
            pins.iter()
                .any(|pin| pin.payload().key() == child_payload.key())
        );

        drop(pins);
        assert_eq!(payload.payload_pin_count(), 0);
        assert_eq!(child_payload.payload_pin_count(), 0);
    }

    #[test]
    fn umount_busy_check_leaves_runtime_open_when_payloads_are_active() {
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
            DevId::new(42),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("busy-root"),
        )
        .expect("root payload");
        assert_eq!(payload.runtime_state(), settlement::MountRuntimeState::Open);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));

        assert_eq!(payload.begin_lazy_detach(), Ok(false));
        assert_eq!(
            payload.runtime_state(),
            settlement::MountRuntimeState::DetachedPending
        );
        drop(pin);
        assert_eq!(
            payload.runtime_state(),
            settlement::MountRuntimeState::Quiescing
        );
        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(
            super::drive_background_mount_settlement_once(&guard),
            StepOutcome::done(())
        );
        assert_eq!(
            payload.runtime_state(),
            settlement::MountRuntimeState::Detached
        );
    }

    #[test]
    fn cloned_mount_namespaces_move_independent_identity_trees() {
        use core::sync::atomic::AtomicBool;

        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_mount_table_for_test();

        let fs = Arc::new(MockFs);
        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(35),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("clone-tree"),
        )
        .expect("mount payload");
        let root_rnode = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("root rnode");
        let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
        let root_mount = MountIdentity::new_cap_with_root_dentry(
            MountId::new(35),
            None,
            root_dentry.clone(),
            None,
            payload.clone(),
            MountFlags::empty(),
        )
        .expect("root mount");
        let parent_namespace = MountNamespace::new_cap(root_mount.clone()).expect("parent ns");

        let make_mountpoint = |id: u64, name: &[u8]| {
            let rnode = RNode::new_cap_in_mount(
                FsObjectId::new(id),
                InodeMeta::new(InodeKind::Directory, 0o040755),
                RNodeBacking::Directory,
                &payload,
            )
            .expect("mountpoint rnode");
            let mut raw = DEntry::new(InlineName::new(name).expect("mountpoint name"), rnode);
            raw.set_parent_hint(&root_dentry);
            crate::vfs::adapter::step_engine::sign(raw).expect("mountpoint dentry")
        };
        let old = make_mountpoint(350, b"old");
        let child_new = make_mountpoint(351, b"child-new");
        let parent_new = make_mountpoint(352, b"parent-new");
        root_dentry.cache_child(old.clone());
        root_dentry.cache_child(child_new.clone());
        root_dentry.cache_child(parent_new.clone());

        let mounted_root_rnode = RNode::new_cap_in_mount(
            FsObjectId::new(353),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("mounted root rnode");
        let mounted_root =
            DEntry::new_cap(InlineName::ROOT, mounted_root_rnode).expect("mounted root dentry");
        let mounted = MountIdentity::new_cap_with_root_dentry(
            MountId::new(36),
            Some(old.clone()),
            mounted_root.clone(),
            Some(root_mount.clone()),
            payload.clone(),
            MountFlags::NOEXEC,
        )
        .expect("mounted identity");
        mounted.set_propagation(Propagation::Shared);
        mounted.set_peer_group(88);
        register_mount(&payload, old.rnode().fs_object_id(), mounted.clone());
        parent_namespace.register_mount(&old, mounted.clone());

        let child_namespace = parent_namespace.clone_ns().expect("clone namespace");
        let child_root_mount = child_namespace.root().clone();
        let child_mounted = child_namespace.mount_for(&old).expect("child mounted row");
        assert_ne!(child_root_mount.key(), root_mount.key());
        assert_ne!(child_root_mount.id(), root_mount.id());
        assert_eq!(
            child_root_mount.root_dentry().key(),
            root_mount.root_dentry().key()
        );
        assert_ne!(child_mounted.key(), mounted.key());
        assert_ne!(child_mounted.id(), mounted.id());
        assert_eq!(
            child_mounted.root_dentry().key(),
            mounted.root_dentry().key()
        );
        assert_eq!(child_mounted.flags(), mounted.flags());
        assert_eq!(child_mounted.propagation(), mounted.propagation());
        assert_eq!(child_mounted.peer_group(), mounted.peer_group());
        assert_eq!(
            child_mounted.parent().map(Cap::key),
            Some(child_root_mount.key())
        );

        let guard = crate::vfs::adapter::step_engine::guard();
        move_mount_in_namespace(
            child_mounted.root_dentry(),
            child_new.clone(),
            &payload,
            &child_namespace,
            &guard,
        )
        .expect("move child namespace mount");
        drop(guard);
        assert!(child_namespace.mount_for(&old).is_none());
        assert_eq!(
            child_namespace
                .mount_for(&child_new)
                .expect("child new row")
                .key(),
            child_mounted.key()
        );
        assert_eq!(
            parent_namespace
                .mount_for(&old)
                .expect("parent old row")
                .key(),
            mounted.key()
        );
        assert!(parent_namespace.mount_for(&child_new).is_none());
        assert_eq!(
            child_namespace
                .dotdot_parent_for_mount_root(child_mounted.root_dentry())
                .expect("child dotdot")
                .key(),
            root_dentry.key()
        );
        assert_eq!(
            parent_namespace
                .dotdot_parent_for_mount_root(mounted.root_dentry())
                .expect("parent dotdot")
                .key(),
            root_dentry.key()
        );
        assert_eq!(
            child_namespace.snapshot_mounts()[0].mountpoint_path,
            b"/child-new"
        );
        assert_eq!(
            parent_namespace.snapshot_mounts()[0].mountpoint_path,
            b"/old"
        );
        assert_eq!(
            mount_for(&payload, old.rnode().fs_object_id())
                .expect("global parent old row")
                .key(),
            mounted.key()
        );
        assert!(mount_for(&payload, child_new.rnode().fs_object_id()).is_none());

        let guard = crate::vfs::adapter::step_engine::guard();
        move_mount_in_namespace(
            mounted.root_dentry(),
            parent_new.clone(),
            &payload,
            &parent_namespace,
            &guard,
        )
        .expect("move parent namespace mount");
        drop(guard);
        assert!(parent_namespace.mount_for(&old).is_none());
        assert_eq!(
            parent_namespace
                .mount_for(&parent_new)
                .expect("parent new row")
                .key(),
            mounted.key()
        );
        assert_eq!(
            child_namespace
                .mount_for(&child_new)
                .expect("child row remains")
                .key(),
            child_mounted.key()
        );
        assert!(child_namespace.mount_for(&parent_new).is_none());
        assert_eq!(
            child_namespace.snapshot_mounts()[0].mountpoint_path,
            b"/child-new"
        );
        assert_eq!(
            parent_namespace.snapshot_mounts()[0].mountpoint_path,
            b"/parent-new"
        );

        let snapshot_ready = AtomicBool::new(false);
        let release_clone = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let clone_handle = scope.spawn(|| {
                parent_namespace
                    .clone_ns_with_snapshot_hook(|| {
                        snapshot_ready.store(true, Ordering::Release);
                        while !release_clone.load(Ordering::Acquire) {
                            core::hint::spin_loop();
                        }
                    })
                    .expect("concurrent namespace clone")
            });

            while !snapshot_ready.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            let guard = crate::vfs::adapter::step_engine::guard();
            move_mount_in_namespace(
                mounted.root_dentry(),
                old.clone(),
                &payload,
                &parent_namespace,
                &guard,
            )
            .expect("move parent after clone snapshot");
            drop(guard);
            release_clone.store(true, Ordering::Release);

            let concurrent_clone = clone_handle.join().expect("clone thread");
            let cloned_before_move = concurrent_clone
                .mount_for(&parent_new)
                .expect("clone retains pre-move row");
            assert_eq!(
                cloned_before_move
                    .mountpoint()
                    .expect("clone placement")
                    .key(),
                parent_new.key()
            );
            assert!(concurrent_clone.mount_for(&old).is_none());
        });
    }

    #[test]
    fn bind_mounts_create_distinct_root_dentry_projections() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_mount_table_for_test();

        let source_fs = Arc::new(MockFs);
        let source_payload = MountPayload::new_cap(
            source_fs.clone() as Arc<dyn FsOps>,
            source_fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(41),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("bind-source"),
        )
        .expect("source payload");
        let source_rnode = RNode::new_cap_in_mount(
            FsObjectId::new(410),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &source_payload,
        )
        .expect("source rnode");
        let source = DEntry::new_cap(
            InlineName::new(b"source").expect("source name"),
            source_rnode,
        )
        .expect("source dentry");

        let target_fs = Arc::new(MockFs);
        let target_payload = MountPayload::new_cap(
            target_fs.clone() as Arc<dyn FsOps>,
            target_fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(42),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("bind-target"),
        )
        .expect("target payload");

        let make_target = |parent_id: u64, target_id: u64, name: &[u8]| {
            let parent_rnode = RNode::new_cap_in_mount(
                FsObjectId::new(parent_id),
                InodeMeta::new(InodeKind::Directory, 0o040755),
                RNodeBacking::Directory,
                &target_payload,
            )
            .expect("target parent rnode");
            let parent =
                DEntry::new_cap(InlineName::ROOT, parent_rnode).expect("target parent dentry");
            let target_rnode = RNode::new_cap_in_mount(
                FsObjectId::new(target_id),
                InodeMeta::new(InodeKind::Directory, 0o040755),
                RNodeBacking::Directory,
                &target_payload,
            )
            .expect("target rnode");
            let mut target_raw =
                DEntry::new(InlineName::new(name).expect("target name"), target_rnode);
            target_raw.set_parent_hint(&parent);
            let target = crate::vfs::adapter::step_engine::sign(target_raw).expect("target dentry");
            (parent, target)
        };
        let (first_parent, first_target) = make_target(420, 421, b"first");
        let (second_parent, second_target) = make_target(430, 431, b"second");
        let guard = crate::vfs::adapter::step_engine::guard();

        let first = bind_mount(source.clone(), first_target, &target_payload, &guard)
            .expect("first bind mount")
            .mount;
        let second = bind_mount(source.clone(), second_target, &target_payload, &guard)
            .expect("second bind mount")
            .mount;

        assert_ne!(first.root_dentry().key(), source.key());
        assert_ne!(second.root_dentry().key(), source.key());
        assert_ne!(first.root_dentry().key(), second.root_dentry().key());
        assert_eq!(first.root_dentry().rnode().key(), source.rnode().key());
        assert_eq!(second.root_dentry().rnode().key(), source.rnode().key());
        assert_eq!(
            dotdot_parent_for_mount_root(first.root_dentry())
                .expect("first bind dotdot parent")
                .key(),
            first_parent.key()
        );
        assert_eq!(
            dotdot_parent_for_mount_root(second.root_dentry())
                .expect("second bind dotdot parent")
                .key(),
            second_parent.key()
        );
    }

    #[test]
    fn mount_namespace_mountpoint_stack_is_lifo_and_umount_restores_lower_mount() {
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
            DevId::new(32),
            MountOptions::default(),
            "mockfs",
            SourceLabel::Static("stack-parent"),
        )
        .expect("parent payload");
        let parent_root = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("parent root");
        let parent_mount = MountIdentity::new_cap(
            MountId::new(32),
            None,
            parent_root,
            None,
            payload.clone(),
            MountFlags::empty(),
        )
        .expect("parent mount");
        let namespace = MountNamespace::new_cap(parent_mount).expect("namespace");
        let mountpoint_rnode = RNode::new_cap_in_mount(
            FsObjectId::new(99),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &payload,
        )
        .expect("mountpoint rnode");
        let mountpoint = DEntry::new_cap(
            InlineName::new(b"stack").expect("mountpoint name"),
            mountpoint_rnode,
        )
        .expect("mountpoint");

        let make_child = |id: u64| {
            let child_fs = Arc::new(MockFs);
            let child_payload = MountPayload::new_cap(
                child_fs.clone() as Arc<dyn FsOps>,
                child_fs as Arc<dyn FsPageBacking>,
                None,
                DevId::new(id as u32),
                MountOptions::default(),
                "mockfs",
                SourceLabel::Static("stack-child"),
            )
            .expect("child payload");
            let child_root = RNode::new_cap_in_mount(
                FsObjectId::ROOT,
                InodeMeta::new(InodeKind::Directory, 0o040755),
                RNodeBacking::Directory,
                &child_payload,
            )
            .expect("child root");
            MountIdentity::new_cap(
                MountId::new(id),
                Some(mountpoint.clone()),
                child_root,
                None,
                child_payload,
                MountFlags::empty(),
            )
            .expect("child mount")
        };
        let lower = make_child(33);
        let upper = make_child(34);

        namespace.register_mount(&mountpoint, lower.clone());
        namespace.register_mount(&mountpoint, upper.clone());
        assert_eq!(
            namespace.mount_for(&mountpoint).expect("upper").id(),
            upper.id()
        );

        namespace.umount(&mountpoint).expect("pop upper");
        assert_eq!(
            namespace.mount_for(&mountpoint).expect("lower").id(),
            lower.id()
        );

        namespace
            .umount(lower.root_dentry())
            .expect("pop lower by mounted root");
        assert!(namespace.mount_for(&mountpoint).is_none());
    }

    #[test]
    fn bootstrap_mount_registers_with_parent_mount_payload_key() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_mount_table_for_test();

        let parent_fs = Arc::new(MockFs);
        let parent_payload = MountPayload::new_cap(
            parent_fs.clone() as Arc<dyn FsOps>,
            parent_fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(11),
            MountOptions::default(),
            "parentfs",
            SourceLabel::Static("parent"),
        )
        .expect("parent mount payload");
        let parent_root = RNode::new_cap_in_mount(
            FsObjectId::ROOT,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &parent_payload,
        )
        .expect("parent root rnode");
        let parent_mount = MountIdentity::new_cap(
            MountId::new(11),
            None,
            parent_root,
            None,
            parent_payload.clone(),
            MountFlags::empty(),
        )
        .expect("parent mount");

        let mountpoint_id = FsObjectId::new(44);
        let mountpoint_rnode = RNode::new_cap_in_mount(
            mountpoint_id,
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
            &parent_payload,
        )
        .expect("mountpoint rnode");
        let mountpoint = DEntry::new_cap(
            crate::vfs::InlineName::new(b"mnt").expect("inline name"),
            mountpoint_rnode,
        )
        .expect("mountpoint dentry");

        let child_fs = Arc::new(MockFs);
        let child_payload = MountPayload::new_cap(
            child_fs.clone() as Arc<dyn FsOps>,
            child_fs as Arc<dyn FsPageBacking>,
            None,
            DevId::new(12),
            MountOptions::default(),
            "childfs",
            SourceLabel::Static("child"),
        )
        .expect("child mount payload");

        let guard = crate::vfs::adapter::step_engine::guard();
        let mounted = bootstrap_mount(
            child_payload.clone(),
            mountpoint,
            Some(parent_mount),
            &guard,
        )
        .expect("bootstrap mount");

        assert_eq!(
            mount_for(&parent_payload, mountpoint_id)
                .expect("parent-keyed mount")
                .id(),
            mounted.id()
        );
        assert!(
            mount_for(&child_payload, mountpoint_id).is_none(),
            "child/source payload must not be used as the mountpoint lookup key"
        );
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

#[cfg(test)]
mod settlement_lifecycle_tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

    use super::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
    use crate::execution::{Errno, Guard};
    use crate::io_manager::page::PageGeneration;
    use crate::mount::settlement::{
        MountRuntimeCell, MountRuntimeState, MountSettlementOp, MountTransactionFrontier,
        SettlementScope,
    };
    use crate::page_backed::{
        ErrorCursor, ErrorSeq, FileFsyncFrontier, Frame, FsPageBacking, PageIndex,
    };
    use crate::sync::SpinMutex;
    use crate::vfs::adapter::step_engine::{Cap, NoProgress, PayloadCap, StepOutcome};
    use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta};

    struct SettlementFs {
        shutdown_count: AtomicU32,
        file_settlement_count: AtomicU32,
        mount_settlement_count: AtomicU32,
        last_file_settlement_object: AtomicU64,
        last_file_settlement_frontier: SpinMutex<Option<FileFsyncFrontier>>,
        last_mount_settlement_frontier: SpinMutex<Option<MountTransactionFrontier>>,
        fail_shutdown: AtomicBool,
        eagain_before_done: AtomicU32,
    }

    impl SettlementFs {
        fn new(fail_shutdown: bool) -> Self {
            Self {
                shutdown_count: AtomicU32::new(0),
                file_settlement_count: AtomicU32::new(0),
                mount_settlement_count: AtomicU32::new(0),
                last_file_settlement_object: AtomicU64::new(0),
                last_file_settlement_frontier: SpinMutex::new(None),
                last_mount_settlement_frontier: SpinMutex::new(None),
                fail_shutdown: AtomicBool::new(fail_shutdown),
                eagain_before_done: AtomicU32::new(0),
            }
        }

        fn shutdown_count(&self) -> u32 {
            self.shutdown_count.load(Ordering::Acquire)
        }

        fn file_settlement_count(&self) -> u32 {
            self.file_settlement_count.load(Ordering::Acquire)
        }

        fn mount_settlement_count(&self) -> u32 {
            self.mount_settlement_count.load(Ordering::Acquire)
        }

        fn last_file_settlement_object(&self) -> FsObjectId {
            FsObjectId::new(self.last_file_settlement_object.load(Ordering::Acquire))
        }

        fn last_file_settlement_frontier(&self) -> Option<FileFsyncFrontier> {
            self.last_file_settlement_frontier.lock().clone()
        }

        fn last_mount_settlement_frontier(&self) -> Option<MountTransactionFrontier> {
            *self.last_mount_settlement_frontier.lock()
        }

        fn set_eagain_before_done(&self, count: u32) {
            self.eagain_before_done.store(count, Ordering::Release);
        }
    }

    impl FsOps for SettlementFs {
        fn lookup(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<FsObjectId, NoProgress> {
            StepOutcome::err(Errno::ENOENT.into())
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
            StepOutcome::err(Errno::EROFS.into())
        }

        fn create_inode(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn unlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn rename(
            &self,
            _old_parent: FsObjectId,
            _old_name: &[u8],
            _new_parent: FsObjectId,
            _new_name: &[u8],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn link(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn mkdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _mode: u16,
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn rmdir(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _target: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn symlink(
            &self,
            _parent: FsObjectId,
            _name: &[u8],
            _link_target: &[u8],
            _cred: &Credential,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
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

        fn settle_file(
            &self,
            fs_object_id: FsObjectId,
            generation_frontier: &FileFsyncFrontier,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.file_settlement_count.fetch_add(1, Ordering::AcqRel);
            self.last_file_settlement_object
                .store(fs_object_id.as_u64(), Ordering::Release);
            *self.last_file_settlement_frontier.lock() = Some(generation_frontier.clone());
            StepOutcome::done(())
        }

        fn settle_mount(
            &self,
            transaction_frontier: MountTransactionFrontier,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.mount_settlement_count.fetch_add(1, Ordering::AcqRel);
            *self.last_mount_settlement_frontier.lock() = Some(transaction_frontier);
            StepOutcome::done(())
        }

        fn shutdown(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            self.shutdown_count.fetch_add(1, Ordering::AcqRel);
            let mut remaining = self.eagain_before_done.load(Ordering::Acquire);
            while remaining != 0 {
                match self.eagain_before_done.compare_exchange_weak(
                    remaining,
                    remaining - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return StepOutcome::continue_with(NoProgress),
                    Err(next) => remaining = next,
                }
            }
            if self.fail_shutdown.load(Ordering::Acquire) {
                StepOutcome::err(Errno::EIO.into())
            } else {
                StepOutcome::done(())
            }
        }
    }

    impl FsPageBacking for SettlementFs {
        fn fetch_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<Frame, NoProgress> {
            StepOutcome::err(Errno::ENOSYS.into())
        }

        fn flush_page(
            &self,
            _fs_object_id: FsObjectId,
            _offset: u64,
            _frame: &Frame,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn truncate(
            &self,
            _fs_object_id: FsObjectId,
            _new_size: u64,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::err(Errno::EROFS.into())
        }

        fn fsync_file(
            &self,
            _fs_object_id: FsObjectId,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    fn settlement_payload(fail_shutdown: bool) -> (Cap<MountPayload>, Arc<SettlementFs>) {
        crate::zones::register_all().expect("kernel zones");
        let fs = Arc::new(SettlementFs::new(fail_shutdown));
        let payload = MountPayload::new_cap(
            fs.clone() as Arc<dyn FsOps>,
            fs.clone() as Arc<dyn FsPageBacking>,
            None,
            DevId::new(71),
            MountOptions::default(),
            "settlementfs",
            SourceLabel::Static("settlement"),
        )
        .expect("mount payload");
        (payload, fs)
    }

    #[test]
    fn syncfs_reports_old_unobserved_error_once() {
        let seq = ErrorSeq::new();
        let mut cursor = ErrorCursor::new();

        seq.record(Errno::EIO);

        assert_eq!(seq.observe(&mut cursor), Some(Errno::EIO));
        assert_eq!(seq.observe(&mut cursor), None);
    }

    #[test]
    fn normal_umount_busy_check_prevents_partial_quiesce() {
        let mut cell = MountRuntimeCell::new();

        assert_eq!(cell.try_claim_settlement(SettlementScope::Detach), Ok(()));
        assert_eq!(cell.state(), MountRuntimeState::Quiescing);
        assert_eq!(
            cell.try_claim_settlement(SettlementScope::Detach),
            Err(Errno::EBUSY)
        );
        cell.complete_settlement(Ok(()));
        assert_eq!(cell.state(), MountRuntimeState::Detached);
    }

    #[test]
    fn lazy_detach_retains_payload_until_background_settlement() {
        let mut cell = MountRuntimeCell::new();

        cell.note_payload_pin_acquired();
        assert_eq!(cell.begin_lazy_detach(), Ok(false));
        assert_eq!(cell.state(), MountRuntimeState::DetachedPending);

        cell.note_payload_pin_released();
        assert_eq!(cell.try_claim_settlement(SettlementScope::Detach), Ok(()));
        cell.complete_settlement(Ok(()));
        assert_eq!(cell.state(), MountRuntimeState::Detached);
    }

    #[test]
    fn lazy_detach_release_queues_and_drives_background_settlement() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(false);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));

        assert_eq!(payload.begin_lazy_detach(), Ok(false));
        assert_eq!(payload.runtime_state(), MountRuntimeState::DetachedPending);

        drop(pin);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Quiescing);
        assert_eq!(payload.payload_pin_count(), 1);
        assert_eq!(super::background_mount_settlement_queue_len(), 1);

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(
            super::drive_background_mount_settlement_once(&guard),
            StepOutcome::done(())
        );

        assert_eq!(fs.shutdown_count(), 1);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Detached);
        assert_eq!(payload.payload_pin_count(), 0);
        assert_eq!(super::background_mount_settlement_queue_len(), 0);
    }

    #[test]
    fn background_mount_settlement_requeues_retryable_detach() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(false);
        fs.set_eagain_before_done(1);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));

        assert_eq!(payload.begin_lazy_detach(), Ok(false));
        drop(pin);
        assert_eq!(super::background_mount_settlement_queue_len(), 1);

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(
            super::drive_background_mount_settlement_once(&guard),
            StepOutcome::err(Errno::EAGAIN.into())
        );
        assert_eq!(payload.runtime_state(), MountRuntimeState::Quiescing);
        assert_eq!(payload.payload_pin_count(), 1);
        assert_eq!(super::background_mount_settlement_queue_len(), 1);

        assert_eq!(
            super::drive_background_mount_settlement_once(&guard),
            StepOutcome::done(())
        );
        assert_eq!(fs.shutdown_count(), 2);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Detached);
        assert_eq!(payload.payload_pin_count(), 0);
        assert_eq!(super::background_mount_settlement_queue_len(), 0);
    }

    #[test]
    fn file_settlement_drives_backend_file_hook_with_object_frontier() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(false);
        let generation_frontier = FileFsyncFrontier::from_pages_for_test(alloc::vec![
            (PageIndex::new(3), PageGeneration::new(7)),
            (PageIndex::new(9), PageGeneration::new(11)),
        ]);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
        let mut op = MountSettlementOp::new(
            pin,
            SettlementScope::File {
                object: FsObjectId::new(12),
                generation_frontier: generation_frontier.clone(),
            },
        )
        .expect("claim file");

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(op.drive(&guard), StepOutcome::done(()));

        assert_eq!(fs.file_settlement_count(), 1);
        assert_eq!(fs.last_file_settlement_object(), FsObjectId::new(12));
        assert_eq!(
            fs.last_file_settlement_frontier(),
            Some(generation_frontier)
        );
        assert_eq!(fs.mount_settlement_count(), 0);
        assert_eq!(fs.shutdown_count(), 0);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Open);
    }

    #[test]
    fn mount_settlement_drives_backend_mount_hook() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(false);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
        let transaction_frontier = MountTransactionFrontier::new(55);
        let mut op = MountSettlementOp::new(
            pin,
            SettlementScope::Mount {
                transaction_frontier,
            },
        )
        .expect("claim mount");

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(op.drive(&guard), StepOutcome::done(()));

        assert_eq!(fs.file_settlement_count(), 0);
        assert_eq!(fs.mount_settlement_count(), 1);
        assert_eq!(
            fs.last_mount_settlement_frontier(),
            Some(transaction_frontier)
        );
        assert_eq!(fs.shutdown_count(), 0);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Open);
    }

    #[test]
    fn default_shutdown_returns_done() {
        tx_test_support::init_host();
        let guard = crate::vfs::adapter::step_engine::guard();
        let fs = SettlementFs::new(false);

        assert_eq!(fs.shutdown(&guard), StepOutcome::done(()));
    }

    #[test]
    fn mount_settlement_op_owns_payload_pin_while_active() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, _fs) = settlement_payload(false);
        assert_eq!(payload.payload_pin_count(), 0);

        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
        let mut op = MountSettlementOp::new(pin, SettlementScope::Detach).expect("claim detach");
        assert_eq!(op.payload_pin_count(), 1);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Quiescing);

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(op.drive(&guard), StepOutcome::done(()));
        assert_eq!(payload.runtime_state(), MountRuntimeState::Detached);

        drop(op);
        assert_eq!(payload.payload_pin_count(), 0);
    }

    #[test]
    fn detach_settlement_calls_backend_shutdown() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(false);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
        let mut op = MountSettlementOp::new(pin, SettlementScope::Detach).expect("claim detach");

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(op.drive(&guard), StepOutcome::done(()));

        assert_eq!(fs.shutdown_count(), 1);
        assert_eq!(payload.runtime_state(), MountRuntimeState::Detached);
    }

    #[test]
    fn detach_settlement_error_records_errseq_and_enters_recovery_only() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let (payload, fs) = settlement_payload(true);
        let pin = MountPayloadPin::acquire(&PayloadCap::from_cap(payload.clone()));
        let mut op = MountSettlementOp::new(pin, SettlementScope::Detach).expect("claim detach");

        let guard = crate::vfs::adapter::step_engine::guard();
        assert_eq!(op.drive(&guard), StepOutcome::err(Errno::EIO.into()));

        assert_eq!(fs.shutdown_count(), 1);
        assert_eq!(payload.runtime_state(), MountRuntimeState::RecoveryOnly);
        assert_eq!(payload.observe_mount_error(), Some(Errno::EIO));
        assert_eq!(payload.observe_mount_error(), None);
    }
}
