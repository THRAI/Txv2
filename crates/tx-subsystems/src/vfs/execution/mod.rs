#![cfg(any(test, feature = "vfs-read-test-support"))]

use alloc::boxed::Box;

use tx_substrate::zone::Cap;

use crate::mount::structure::MountIdentity;
use crate::page_backed::{create_file_page_container, Frame};
use crate::step::{Errno, Progress, StepOutcome};
use crate::vfs::checks::require::ResolveCtx;
#[cfg(test)]
use crate::vfs::checks::witness::ParentAndName;
use crate::vfs::checks::witness::ParentAndNamedChild;
use crate::vfs::fs_ops::RNodeBackingInit;
#[cfg(test)]
use crate::vfs::fs_ops::{CapSet, Credential};
#[cfg(test)]
use crate::vfs::structure::DEntryChildrenInsertReservation;
#[cfg(test)]
use crate::vfs::structure::RNode;
use crate::vfs::structure::{
    create_dentry_for_create_lane, create_open_file_for_create_lane, create_rnode_for_create_lane,
    DEntry, FsObjectId, InodeMeta, NameOwned, NewDEntrySpec, NewOpenFileSpec, NewRNodeSpec,
    OpenFile, OpenFileKey, OpenFlags, ProjectionKey, ProjectionReadCtx, ProjectionSchema,
    RNodeBacking, RenderBuffer, StructPayload,
};

#[derive(Clone)]
#[cfg(test)]
struct OpenCreateInsertReady {
    parent: Cap<DEntry>,
    parent_mount: Cap<MountIdentity>,
    parent_fs_object_id: FsObjectId,
    name: NameOwned,
}

#[cfg(test)]
struct OpenCreatePrepared {
    child_rnode: Cap<RNode>,
    child_dentry: Cap<DEntry>,
}

#[cfg(test)]
enum OpenCreateResume {
    BackendCreateInFlight(OpenCreateInsertReady),
}

#[cfg(test)]
pub(crate) struct OpenCreateOperation {
    path: NameOwnedPath,
    resume: Option<OpenCreateResume>,
}

struct NameOwnedPath {
    bytes: [u8; 256],
    len: u16,
}

impl NameOwnedPath {
    fn from_bytes(path: &[u8]) -> Result<Self, Errno> {
        if path.len() > 256 {
            return Err(Errno::NameTooLong);
        }

        let mut bytes = [0; 256];
        bytes[..path.len()].copy_from_slice(path);
        Ok(Self {
            bytes,
            len: path.len() as u16,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

#[cfg(test)]
impl OpenCreateOperation {
    pub(crate) fn new(path: &[u8]) -> Result<Self, Errno> {
        Ok(Self {
            path: NameOwnedPath::from_bytes(path)?,
            resume: None,
        })
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.resume.is_some()
    }
}

#[cfg(test)]
fn default_create_lane_credential() -> Credential {
    Credential {
        uid: 0,
        gid: 0,
        egid: 0,
        groups: [0; 32],
        group_count: 0,
        capabilities: CapSet { bits: 0 },
    }
}

#[cfg(test)]
fn upgrade_open_create_parent<'g>(
    target: ParentAndName<'g>,
) -> Result<OpenCreateInsertReady, Errno> {
    let parent_fs_object_id = target.parent.rnode.fs_object_id;
    let parent = target.parent.to_cap().map_err(|_| Errno::Stale)?;
    let parent_mount = target.parent_mount.to_cap().map_err(|_| Errno::Stale)?;

    Ok(OpenCreateInsertReady {
        parent,
        parent_mount,
        parent_fs_object_id,
        name: target.name,
    })
}

#[cfg(test)]
fn upgrade_open_create_parent_for_resume<'g>(
    target: ParentAndName<'g>,
    resume: &mut Option<OpenCreateResume>,
) -> Result<OpenCreateInsertReady, Errno> {
    let ready = upgrade_open_create_parent(target)?;

    if let Some(OpenCreateResume::BackendCreateInFlight(saved)) = resume.as_ref() {
        let same_context = ready.parent.raw() == saved.parent.raw()
            && ready.parent_mount.raw() == saved.parent_mount.raw()
            && ready.parent_fs_object_id == saved.parent_fs_object_id
            && ready.name == saved.name;

        if !same_context {
            *resume = None;
            return Err(Errno::Stale);
        }
    }

    Ok(ready)
}

#[cfg(test)]
fn with_open_create_insert_reservation<R>(
    ready: &OpenCreateInsertReady,
    f: impl FnOnce(&OpenCreateInsertReady, DEntryChildrenInsertReservation<'_>) -> StepOutcome<R>,
) -> StepOutcome<R> {
    let reservation = ready
        .parent
        .children
        .reserve_insert(ready.name.clone())
        .map_err(map_children_insert_error);

    let reservation = match reservation {
        Ok(reservation) => reservation,
        Err(errno) => return StepOutcome::Err(errno),
    };

    f(ready, reservation)
}

#[cfg(test)]
fn map_children_insert_error(err: crate::vfs::structure::DEntryChildrenInstallError) -> Errno {
    match err {
        crate::vfs::structure::DEntryChildrenInstallError::AlreadyPresent => Errno::Busy,
        crate::vfs::structure::DEntryChildrenInstallError::Full => Errno::Busy,
        crate::vfs::structure::DEntryChildrenInstallError::Busy => Errno::Busy,
        crate::vfs::structure::DEntryChildrenInstallError::Missing => Errno::Stale,
    }
}

fn map_zone_error(_err: tx_substrate::zone::ZoneError) -> Errno {
    Errno::Busy
}

#[cfg(test)]
fn derive_create_lane_rnode_key(object_id: FsObjectId) -> crate::vfs::structure::RNodeKey {
    crate::vfs::structure::RNodeKey(object_id.0)
}

#[cfg(test)]
fn derive_create_lane_dentry_key(
    ready: &OpenCreateInsertReady,
    object_id: FsObjectId,
) -> crate::vfs::structure::DEntryKey {
    crate::vfs::structure::DEntryKey(object_id.0 ^ u64::from(ready.name.len))
}

#[cfg(test)]
fn derive_create_lane_open_file_key(object_id: FsObjectId) -> OpenFileKey {
    OpenFileKey(object_id.0 ^ 0x0f0f_0f0f_0f0f_0f0f)
}

fn derive_read_lane_rnode_key(object_id: FsObjectId) -> crate::vfs::structure::RNodeKey {
    crate::vfs::structure::RNodeKey(object_id.0 ^ 0x1010_2020_3030_4040)
}

fn derive_read_lane_dentry_key(
    parent: &Cap<DEntry>,
    name: &NameOwned,
    object_id: FsObjectId,
) -> crate::vfs::structure::DEntryKey {
    crate::vfs::structure::DEntryKey(
        object_id.0 ^ ((parent.raw() as u64) << 16) ^ u64::from(name.len),
    )
}

fn derive_read_lane_open_file_key(object_id: FsObjectId) -> OpenFileKey {
    OpenFileKey(object_id.0 ^ 0xa5a5_5a5a_1111_eeee)
}

#[cfg(test)]
fn request_backend_create_inode(
    ready: &OpenCreateInsertReady,
    reservation: &DEntryChildrenInsertReservation<'_>,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> StepOutcome<(FsObjectId, InodeMeta)> {
    let payload = ready.parent_mount.payload.attached_cap();
    let payload = match payload {
        Some(payload) => payload,
        None => return StepOutcome::Err(Errno::NotImplemented),
    };
    let cred = default_create_lane_credential();
    payload.fs_ops.create_inode(
        ready.parent_fs_object_id,
        reservation.key().as_bytes(),
        InodeMeta::TYPE_REGULAR | 0o644,
        &cred,
        guard,
    )
}

#[cfg(test)]
fn prepare_create_lane_objects(
    ready: &OpenCreateInsertReady,
    reservation: &DEntryChildrenInsertReservation<'_>,
    fs_object_id: FsObjectId,
    inode_meta: InodeMeta,
) -> Result<OpenCreatePrepared, Errno> {
    let child_rnode = create_rnode_for_create_lane(NewRNodeSpec {
        key: derive_create_lane_rnode_key(fs_object_id),
        fs_object_id,
        meta: inode_meta,
        backing: RNodeBacking::StructBacked {
            payload: crate::vfs::structure::StructPayload::Deferred,
        },
    })
    .map_err(map_zone_error)?;
    let child_dentry = create_dentry_for_create_lane(NewDEntrySpec {
        key: derive_create_lane_dentry_key(ready, fs_object_id),
        name: reservation.key().clone(),
        rnode: child_rnode.clone(),
    })
    .map_err(map_zone_error)?;

    Ok(OpenCreatePrepared {
        child_rnode,
        child_dentry,
    })
}

#[cfg(test)]
fn publish_open_file_for_create_lane(
    ready: &OpenCreateInsertReady,
    prepared: OpenCreatePrepared,
) -> Result<Cap<OpenFile>, Errno> {
    let payload = ready
        .parent_mount
        .payload
        .attached_cap()
        .ok_or(Errno::NotImplemented)?;
    let pin = crate::mount::structure::MountPayloadPin::acquire(&payload);
    create_open_file_for_create_lane(NewOpenFileSpec {
        key: derive_create_lane_open_file_key(prepared.child_rnode.fs_object_id),
        rnode: prepared.child_rnode,
        mount: ready.parent_mount.clone(),
        mount_payload_pin: pin,
        offset: 0,
        flags: OpenFlags {
            read: true,
            write: false,
            append: false,
            nonblock: false,
        },
    })
    .map_err(map_zone_error)
}

fn map_fs_lookup_errno(errno: Errno) -> Errno {
    match errno {
        Errno::NoEntry => Errno::NoEntry,
        Errno::NotDirectory => Errno::NotDirectory,
        Errno::Stale => Errno::Stale,
        Errno::Busy => Errno::Busy,
        _ => Errno::Invalid,
    }
}

fn map_page_container_error(_err: tx_substrate::zone::ZoneError) -> Errno {
    Errno::Busy
}

fn materialize_backing_from_init(init: RNodeBackingInit) -> RNodeBacking {
    match init {
        RNodeBackingInit::PageBacked { pc } => RNodeBacking::PageBacked { pc },
        RNodeBackingInit::StructBacked { payload } => RNodeBacking::StructBacked { payload },
        RNodeBackingInit::Projected { schema, key } => RNodeBacking::Projected { schema, key },
    }
}

fn fallback_read_lane_backing(
    payload: &Cap<crate::mount::structure::MountPayload>,
    fs_object_id: FsObjectId,
    inode_meta: InodeMeta,
) -> Result<RNodeBacking, Errno> {
    if inode_meta.is_regular_file() {
        let pc = create_file_page_container(payload.clone(), fs_object_id)
            .map_err(map_page_container_error)?;
        Ok(RNodeBacking::PageBacked { pc })
    } else {
        Ok(RNodeBacking::StructBacked {
            payload: StructPayload::Deferred,
        })
    }
}

fn read_lane_backing(
    payload: &Cap<crate::mount::structure::MountPayload>,
    fs_object_id: FsObjectId,
    inode_meta: InodeMeta,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Result<RNodeBacking, Errno> {
    match payload.fs_ops.backing_for(fs_object_id, guard) {
        StepOutcome::Done(init) => Ok(materialize_backing_from_init(init)),
        StepOutcome::Err(Errno::NotImplemented) => {
            fallback_read_lane_backing(payload, fs_object_id, inode_meta)
        }
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Blocked(_, _)
        | StepOutcome::Advanced(_)
        | StepOutcome::AdvancedThenBlocked(_, _, _) => Err(Errno::NotImplemented),
    }
}

fn prepare_read_lane_child(
    parent: &Cap<DEntry>,
    mount: &Cap<MountIdentity>,
    name: &NameOwned,
    fs_object_id: FsObjectId,
    inode_meta: InodeMeta,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Result<Cap<DEntry>, Errno> {
    let payload = mount.payload.attached_cap().ok_or(Errno::NotImplemented)?;
    let backing = read_lane_backing(&payload, fs_object_id, inode_meta, guard)?;
    let child_rnode = create_rnode_for_create_lane(NewRNodeSpec {
        key: derive_read_lane_rnode_key(fs_object_id),
        fs_object_id,
        meta: inode_meta,
        backing,
    })
    .map_err(map_zone_error)?;
    create_dentry_for_create_lane(NewDEntrySpec {
        key: derive_read_lane_dentry_key(parent, name, fs_object_id),
        name: name.clone(),
        rnode: child_rnode,
    })
    .map_err(map_zone_error)
}

fn publish_open_file_for_read_lane(
    entity: crate::vfs::checks::witness::EntityAtPath<'_>,
) -> Result<Cap<OpenFile>, Errno> {
    let payload = entity
        .mount
        .payload
        .attached_cap()
        .ok_or(Errno::NotImplemented)?;
    let pin = crate::mount::structure::MountPayloadPin::acquire(&payload);
    let rnode = entity.dentry.rnode.clone();
    let mount = entity.mount.to_cap().map_err(|_| Errno::Stale)?;
    create_open_file_for_create_lane(NewOpenFileSpec {
        key: derive_read_lane_open_file_key(entity.rnode.fs_object_id),
        rnode,
        mount,
        mount_payload_pin: pin,
        offset: 0,
        flags: OpenFlags {
            read: true,
            write: false,
            append: false,
            nonblock: false,
        },
    })
    .map_err(map_zone_error)
}

#[cfg(test)]
fn step_open_create<'g>(
    target: ParentAndName<'g>,
    guard: &'g tx_substrate::epoch::Guard<'g>,
    resume: &mut Option<OpenCreateResume>,
) -> StepOutcome<Cap<OpenFile>> {
    let ready = match upgrade_open_create_parent_for_resume(target, resume) {
        Ok(ready) => ready,
        Err(errno) => return StepOutcome::Err(errno),
    };

    with_open_create_insert_reservation(&ready, |ready, reservation| {
        let (fs_object_id, inode_meta) =
            match request_backend_create_inode(ready, &reservation, guard) {
                StepOutcome::Done(created) => created,
                StepOutcome::Blocked(carrier, interests) => {
                    *resume = Some(OpenCreateResume::BackendCreateInFlight(ready.clone()));
                    return StepOutcome::Blocked(carrier, interests);
                }
                StepOutcome::Err(errno) => {
                    *resume = None;
                    return StepOutcome::Err(errno);
                }
                StepOutcome::Advanced(progress) => {
                    *resume = Some(OpenCreateResume::BackendCreateInFlight(ready.clone()));
                    return StepOutcome::Advanced(progress);
                }
                StepOutcome::AdvancedThenBlocked(progress, carrier, interests) => {
                    *resume = Some(OpenCreateResume::BackendCreateInFlight(ready.clone()));
                    return StepOutcome::AdvancedThenBlocked(progress, carrier, interests);
                }
            };
        let prepared =
            match prepare_create_lane_objects(ready, &reservation, fs_object_id, inode_meta) {
                Ok(prepared) => prepared,
                Err(errno) => {
                    *resume = None;
                    return StepOutcome::Err(errno);
                }
            };
        reservation.commit(prepared.child_dentry.clone());
        *resume = None;
        match publish_open_file_for_create_lane(ready, prepared) {
            Ok(open) => StepOutcome::Done(open),
            Err(errno) => StepOutcome::Err(errno),
        }
    })
}

#[cfg(test)]
pub(crate) fn drive_open_create(
    op: &mut OpenCreateOperation,
    ctx: &ResolveCtx,
) -> StepOutcome<Cap<OpenFile>> {
    let guard = tx_substrate::epoch::guard();
    let witness =
        match crate::vfs::checks::require::require_parent_and_name(op.path.as_bytes(), ctx, &guard)
        {
            Ok(witness) => witness,
            Err(errno) => return StepOutcome::Err(errno),
        };

    step_open_create(witness, &guard, &mut op.resume)
}

#[cfg(test)]
pub(crate) fn drive_open_create_until_boundary(
    op: &mut OpenCreateOperation,
    ctx: &ResolveCtx,
) -> StepOutcome<Cap<OpenFile>> {
    let mut advanced_units = 0usize;

    loop {
        match drive_open_create(op, ctx) {
            StepOutcome::Advanced(Progress::Units(units)) => {
                advanced_units += units;
            }
            StepOutcome::Blocked(carrier, interests) => {
                return if advanced_units == 0 {
                    StepOutcome::Blocked(carrier, interests)
                } else {
                    StepOutcome::AdvancedThenBlocked(
                        Progress::Units(advanced_units),
                        carrier,
                        interests,
                    )
                };
            }
            StepOutcome::AdvancedThenBlocked(Progress::Units(units), carrier, interests) => {
                return StepOutcome::AdvancedThenBlocked(
                    Progress::Units(advanced_units + units),
                    carrier,
                    interests,
                );
            }
            StepOutcome::Done(open) => return StepOutcome::Done(open),
            StepOutcome::Err(errno) => {
                return if advanced_units == 0 {
                    StepOutcome::Err(errno)
                } else {
                    StepOutcome::Advanced(Progress::Units(advanced_units))
                };
            }
        }
    }
}

pub(crate) struct OpenReadOperation<'a> {
    path: NameOwnedPath,
    open: Option<Cap<OpenFile>>,
    request: ReadRequestState<'a>,
    resume: Option<ReadResume>,
}

impl<'a> OpenReadOperation<'a> {
    pub(crate) fn new(path: &[u8], offset: u64, target: &'a mut [u8]) -> Result<Self, Errno> {
        Ok(Self {
            path: NameOwnedPath::from_bytes(path)?,
            open: None,
            request: ReadRequestState::new(offset, target)?,
            resume: None,
        })
    }
}

struct ReadRequestState<'a> {
    start_offset: u64,
    remaining_len: usize,
    target: ReadBuffer<'a>,
    filled: usize,
}

impl<'a> ReadRequestState<'a> {
    const MAX_READ_LEN: usize = crate::page_backed::FRAME_CAPACITY * 2;

    fn new(start_offset: u64, target: &'a mut [u8]) -> Result<Self, Errno> {
        let remaining_len = target.len();
        if remaining_len > Self::MAX_READ_LEN {
            return Err(Errno::Busy);
        }

        Ok(Self {
            start_offset,
            remaining_len,
            target: ReadBuffer::new(target),
            filled: 0,
        })
    }

    fn current_offset(&self) -> u64 {
        self.start_offset + self.filled as u64
    }

    fn remaining(&self) -> usize {
        self.remaining_len.saturating_sub(self.filled)
    }

    fn is_complete(&self) -> bool {
        self.remaining() == 0
    }

    fn append_from_frame(&mut self, frame: &Frame, page_offset: usize) -> usize {
        let chunk = frame.slice(page_offset, self.remaining());
        let len = chunk.len();
        self.target.write(self.filled, chunk);
        self.filled += len;
        len
    }

    fn append_from_slice(&mut self, chunk: &[u8]) -> usize {
        let len = core::cmp::min(chunk.len(), self.remaining());
        self.target.write(self.filled, &chunk[..len]);
        self.filled += len;
        len
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadResume {
    target: ReadFetchTarget,
    fetch_state: ReadFetchState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadFetchTarget {
    request_offset: u64,
    page_base: u64,
    page_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadFetchState {
    NeedPage,
    WaitingPage,
}

enum ReadFetchBoundary {
    Ready(Box<Frame>),
    Blocked {
        target: ReadFetchTarget,
        _carry_progress: usize,
        carrier: crate::step::WakeCarrier,
        interests: crate::step::InterestConditions,
    },
    Err(Errno),
}

struct ReadBuffer<'a> {
    bytes: &'a mut [u8],
}

impl<'a> ReadBuffer<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }

    fn write(&mut self, start: usize, chunk: &[u8]) {
        self.bytes[start..start + chunk.len()].copy_from_slice(chunk);
    }
}

fn ensure_resolved_entity<'g>(
    path: &[u8],
    ctx: &ResolveCtx,
    guard: &'g tx_substrate::epoch::Guard<'g>,
) -> Result<crate::vfs::checks::witness::EntityAtPath<'g>, Errno> {
    match crate::vfs::checks::require::require_entity(path, ctx, guard) {
        Ok(entity) => Ok(entity),
        Err(Errno::NotImplemented) => {
            let state = crate::vfs::checks::resolution::state::make_initial_walk_state(
                path,
                ctx.root_ctx_caps(),
                guard,
            )?;
            let mut step = crate::vfs::checks::resolution::driver::run_walker(
                crate::vfs::checks::resolution::state::WalkMode::Entity,
                state,
                guard,
            );

            loop {
                match step {
                    crate::vfs::checks::resolution::driver::DriverStep::Accept(witness) => {
                        return match *witness {
                            crate::vfs::checks::witness::WalkWitness::Entity(entity) => Ok(entity),
                            _ => Err(Errno::Invalid),
                        };
                    }
                    crate::vfs::checks::resolution::driver::DriverStep::Error(errno) => {
                        return Err(errno)
                    }
                    crate::vfs::checks::resolution::driver::DriverStep::NeedIO(request, token) => {
                        match request.as_ref() {
                            crate::vfs::checks::resolution::step::IORequest::LookupChild {
                                _parent_name: _,
                            } => {}
                        }

                        let (parent, name, current_mount) = match token.as_ref() {
                            crate::vfs::checks::resolution::state::ResumeToken::LookupChild {
                                suspended,
                                parent,
                                name,
                                ..
                            } => (
                                parent.clone(),
                                name.clone(),
                                suspended.current_mount.clone(),
                            ),
                        };

                        let parent_ref = parent.ident_ref(guard);
                        let mount = current_mount.ident_ref(guard);
                        let payload = mount.payload.attached_cap().ok_or(Errno::NotImplemented)?;

                        let fs_object_id = match payload.fs_ops.lookup(
                            parent_ref.rnode.fs_object_id,
                            name.as_bytes(),
                            guard,
                        ) {
                            StepOutcome::Done(id) => id,
                            StepOutcome::Err(errno) => return Err(map_fs_lookup_errno(errno)),
                            StepOutcome::Blocked(_, _)
                            | StepOutcome::Advanced(_)
                            | StepOutcome::AdvancedThenBlocked(_, _, _) => {
                                return Err(Errno::NotImplemented)
                            }
                        };

                        let inode_meta = match payload.fs_ops.load_inode_meta(fs_object_id, guard) {
                            StepOutcome::Done(meta) => meta,
                            StepOutcome::Err(errno) => return Err(map_fs_lookup_errno(errno)),
                            StepOutcome::Blocked(_, _)
                            | StepOutcome::Advanced(_)
                            | StepOutcome::AdvancedThenBlocked(_, _, _) => {
                                return Err(Errno::NotImplemented)
                            }
                        };

                        let child = prepare_read_lane_child(
                            &parent,
                            &current_mount,
                            &name,
                            fs_object_id,
                            inode_meta,
                            guard,
                        )?;
                        step = crate::vfs::checks::resolution::driver::resume_walker(
                            crate::vfs::checks::resolution::state::WalkMode::Entity,
                            *token,
                            crate::vfs::checks::resolution::driver::IOResult::ChildFound(child),
                            guard,
                        );
                    }
                }
            }
        }
        Err(errno) => Err(errno),
    }
}

fn ensure_open_for_read(
    op: &mut OpenReadOperation<'_>,
    ctx: &ResolveCtx,
) -> Result<Cap<OpenFile>, Errno> {
    if let Some(open) = op.open.as_ref() {
        return Ok(open.clone());
    }

    let guard = tx_substrate::epoch::guard();
    let entity = ensure_resolved_entity(op.path.as_bytes(), ctx, &guard)?;
    let open = publish_open_file_for_read_lane(entity)?;
    op.open = Some(open.clone());
    Ok(open)
}

#[cfg(test)]
pub(crate) fn drive_open_read_first_page(
    op: &mut OpenReadOperation<'_>,
    ctx: &ResolveCtx,
) -> StepOutcome<Frame> {
    let open = match ensure_open_for_read(op, ctx) {
        Ok(open) => open,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let guard = tx_substrate::epoch::guard();
    let open_ref = open.ident_ref(&guard);
    let payload = match open_ref.mount.payload.attached_cap() {
        Some(payload) => payload,
        None => return StepOutcome::Err(Errno::NotImplemented),
    };
    let page_container = match &open_ref.rnode.backing {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        RNodeBacking::StructBacked { .. } | RNodeBacking::Projected { .. } => {
            return StepOutcome::Err(Errno::NotImplemented)
        }
    };
    let fs_object_id = match page_container.ident_ref(&guard).file_backing() {
        Some(file) => file.fs_object_id,
        None => return StepOutcome::Err(Errno::Invalid),
    };
    payload.fs_page_backing.fetch_page(fs_object_id, 0, &guard)
}

fn complete_read(op: &OpenReadOperation<'_>) -> usize {
    op.request.filled
}

fn current_read_fetch_target(op: &OpenReadOperation<'_>) -> ReadFetchTarget {
    let request_offset = op
        .resume
        .as_ref()
        .map(|resume| resume.target.request_offset)
        .unwrap_or_else(|| op.request.current_offset());
    let page_base = request_offset & !((crate::page_backed::FRAME_CAPACITY as u64) - 1);
    let page_offset = (request_offset - page_base) as usize;

    ReadFetchTarget {
        request_offset,
        page_base,
        page_offset,
    }
}

fn fetch_page_until_boundary(
    backing: &dyn crate::page_backed::FsPageBacking,
    fs_object_id: FsObjectId,
    target: ReadFetchTarget,
    _initial_fetch_state: ReadFetchState,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> ReadFetchBoundary {
    let mut carry_progress = 0usize;

    loop {
        match backing.fetch_page(fs_object_id, target.page_base, guard) {
            StepOutcome::Done(frame) => return ReadFetchBoundary::Ready(Box::new(frame)),
            StepOutcome::Blocked(carrier, interests) => {
                return ReadFetchBoundary::Blocked {
                    target,
                    _carry_progress: carry_progress,
                    carrier,
                    interests,
                }
            }
            StepOutcome::Advanced(Progress::Units(units)) => {
                carry_progress += units;
            }
            StepOutcome::AdvancedThenBlocked(Progress::Units(units), carrier, interests) => {
                return ReadFetchBoundary::Blocked {
                    target,
                    _carry_progress: carry_progress + units,
                    carrier,
                    interests,
                }
            }
            StepOutcome::Err(errno) => return ReadFetchBoundary::Err(errno),
        }
    }
}

fn drive_projected_read(
    op: &mut OpenReadOperation<'_>,
    schema: &'static dyn ProjectionSchema,
    key: ProjectionKey,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> StepOutcome<usize> {
    let ctx = ProjectionReadCtx::new(guard);
    let mut rendered = RenderBuffer::new();
    if let Err(errno) = schema.render(&key, &ctx, &mut rendered, guard) {
        op.resume = None;
        return StepOutcome::Err(errno);
    }

    let chunk = rendered.slice_from(op.request.current_offset(), op.request.remaining());
    op.request.append_from_slice(chunk);
    op.resume = None;
    StepOutcome::Done(complete_read(op))
}

pub(crate) fn drive_open_read(
    op: &mut OpenReadOperation<'_>,
    ctx: &ResolveCtx,
) -> StepOutcome<usize> {
    let open = match ensure_open_for_read(op, ctx) {
        Ok(open) => open,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let guard = tx_substrate::epoch::guard();
    let open_ref = open.ident_ref(&guard);
    let page_container = match &open_ref.rnode.backing {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        RNodeBacking::Projected { schema, key } => {
            return drive_projected_read(op, *schema, *key, &guard);
        }
        RNodeBacking::StructBacked { .. } => {
            op.resume = None;
            return StepOutcome::Err(Errno::NotImplemented);
        }
    };
    let payload = match open_ref.mount.payload.attached_cap() {
        Some(payload) => payload,
        None => return StepOutcome::Err(Errno::NotImplemented),
    };
    let fs_object_id = match page_container.ident_ref(&guard).file_backing() {
        Some(file) => file.fs_object_id,
        None => return StepOutcome::Err(Errno::Invalid),
    };

    let mut advanced = 0usize;

    loop {
        if op.request.is_complete() {
            op.resume = None;
            return StepOutcome::Done(complete_read(op));
        }

        let fetch_state = op
            .resume
            .as_ref()
            .map(|resume| resume.fetch_state)
            .unwrap_or(ReadFetchState::NeedPage);
        let target = current_read_fetch_target(op);

        match fetch_page_until_boundary(
            &*payload.fs_page_backing,
            fs_object_id,
            target,
            fetch_state,
            &guard,
        ) {
            ReadFetchBoundary::Ready(frame) => {
                let copied = op.request.append_from_frame(&frame, target.page_offset);
                op.resume = None;
                if copied == 0 {
                    return StepOutcome::Done(complete_read(op));
                }
                advanced += copied;
            }
            ReadFetchBoundary::Blocked {
                target,
                _carry_progress: _,
                carrier,
                interests,
            } => {
                op.resume = Some(ReadResume {
                    target,
                    fetch_state: ReadFetchState::WaitingPage,
                });
                return if advanced == 0 {
                    StepOutcome::Blocked(carrier, interests)
                } else {
                    StepOutcome::AdvancedThenBlocked(Progress::Units(advanced), carrier, interests)
                };
            }
            ReadFetchBoundary::Err(errno) => {
                op.resume = None;
                return if advanced == 0 {
                    StepOutcome::Err(errno)
                } else {
                    StepOutcome::Done(complete_read(op))
                };
            }
        }
    }
}

pub fn step_unlink<'g>(_target: ParentAndNamedChild<'g>) -> StepOutcome<()> {
    StepOutcome::Err(Errno::NotImplemented)
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub mod read_harness {
    use super::*;
    use crate::mount::structure::testing::make_bootstrap_pair_with_backend_for_test;
    use crate::mount::structure::{MountIdentity, MountNamespace, MountPayload};
    use crate::vfs::checks::resolution::state::RootCtxCaps;
    use crate::vfs::fs_ops::MountOutput;
    use crate::vfs::structure::{
        DEntryChildLookup, DEntryKey, RNodeFileType, RNodeKey, StructPayload,
    };
    use core::sync::atomic::{AtomicBool, Ordering};

    static HARNESS_LOCK: AtomicBool = AtomicBool::new(false);

    pub struct VfsReadHarness {
        ctx: ResolveCtx,
        root: Cap<DEntry>,
        _mount: Cap<MountIdentity>,
        _namespace: Cap<MountNamespace>,
        _payload: Cap<MountPayload>,
        _serial: HarnessSerialGuard,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MaterializedPathInfo {
        pub fs_object_id: FsObjectId,
        pub file_type: RNodeFileType,
        pub is_page_backed: bool,
    }

    impl VfsReadHarness {
        pub fn from_mount_output(output: MountOutput) -> Result<Self, Errno> {
            let serial = HarnessSerialGuard::acquire();
            setup_zones();
            let MountOutput {
                fs_ops,
                fs_page_backing,
                root_fs_object_id,
                root_inode_meta,
            } = output;

            let root_rnode = create_rnode_for_create_lane(NewRNodeSpec {
                key: RNodeKey(root_fs_object_id.0),
                fs_object_id: root_fs_object_id,
                meta: root_inode_meta,
                backing: RNodeBacking::StructBacked {
                    payload: StructPayload::Deferred,
                },
            })
            .map_err(map_zone_error)?;
            let root = create_dentry_for_create_lane(NewDEntrySpec {
                key: DEntryKey(root_fs_object_id.0),
                name: NameOwned::from_component(b".")?,
                rnode: root_rnode,
            })
            .map_err(map_zone_error)?;

            let (mount, namespace, payload) =
                make_bootstrap_pair_with_backend_for_test(root.clone(), fs_ops, fs_page_backing);
            let ctx = ResolveCtx::new(RootCtxCaps {
                mnt_ns: namespace.clone(),
                mnt_ns_root: root.clone(),
                chroot: None,
                cwd: root.clone(),
                root_mount: mount.clone(),
                cwd_mount: mount.clone(),
            });

            Ok(Self {
                ctx,
                root,
                _mount: mount,
                _namespace: namespace,
                _payload: payload,
                _serial: serial,
            })
        }

        pub fn drive_read(
            &self,
            path: &[u8],
            offset: u64,
            target: &mut [u8],
        ) -> StepOutcome<usize> {
            let mut op = match OpenReadOperation::new(path, offset, target) {
                Ok(op) => op,
                Err(errno) => return StepOutcome::Err(errno),
            };
            drive_open_read(&mut op, &self.ctx)
        }

        pub fn materialized_path_info(
            &self,
            path: &[u8],
        ) -> Result<Option<MaterializedPathInfo>, Errno> {
            let guard = tx_substrate::epoch::guard();
            if path.is_empty() || path == b"/" {
                let root = self.root.ident_ref(&guard);
                return Ok(Some(path_info(&root)));
            }

            let mut cursor = 0usize;
            let mut current = self.root.clone();
            loop {
                while cursor < path.len() && path[cursor] == b'/' {
                    cursor += 1;
                }
                if cursor >= path.len() {
                    let current_ref = current.ident_ref(&guard);
                    return Ok(Some(path_info(&current_ref)));
                }

                let start = cursor;
                while cursor < path.len() && path[cursor] != b'/' {
                    cursor += 1;
                }
                let name = NameOwned::from_component(&path[start..cursor])?;
                let current_ref = current.ident_ref(&guard);
                let child = match current_ref.children.lookup(&name, &guard) {
                    DEntryChildLookup::Found(child) => (*child).into_ident_ref(),
                    DEntryChildLookup::Missing => return Ok(None),
                };

                while cursor < path.len() && path[cursor] == b'/' {
                    cursor += 1;
                }
                if cursor >= path.len() {
                    return Ok(Some(path_info(&child)));
                }

                current = child.to_cap().map_err(|_| Errno::Stale)?;
            }
        }
    }

    fn setup_zones() {
        tx_substrate::testing::init_host_for_test_once();
        let _ = tx_substrate::zone::register_zone_for::<crate::vfs::structure::RNode>();
        let _ = tx_substrate::zone::register_zone_for::<crate::vfs::structure::DEntry>();
        let _ = tx_substrate::zone::register_zone_for::<crate::vfs::structure::OpenFile>();
        let _ = tx_substrate::zone::register_zone_for::<crate::mount::structure::MountIdentity>();
        let _ = tx_substrate::zone::register_zone_for::<crate::mount::structure::MountNamespace>();
        let _ = tx_substrate::zone::register_zone_for::<crate::mount::structure::MountPayload>();
        let _ = tx_substrate::zone::register_zone_for::<crate::page_backed::PageContainer>();
    }

    fn path_info(dentry: &tx_substrate::zone::IdentRef<'_, DEntry>) -> MaterializedPathInfo {
        MaterializedPathInfo {
            fs_object_id: dentry.rnode.fs_object_id,
            file_type: dentry.rnode.meta.file_type(),
            is_page_backed: matches!(dentry.rnode.backing, RNodeBacking::PageBacked { .. }),
        }
    }

    struct HarnessSerialGuard;

    impl HarnessSerialGuard {
        fn acquire() -> Self {
            while HARNESS_LOCK
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
            Self
        }
    }

    impl Drop for HarnessSerialGuard {
        fn drop(&mut self) {
            HARNESS_LOCK.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests;
