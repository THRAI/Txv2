use core::sync::atomic::{AtomicU64, Ordering};

use tx_substrate::zone::Cap;
use tx_substrate::zone::PayloadCap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::facade::SocketHandleFlags;
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::structure::{SocketIdentity, ValidSocketType};
use crate::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking, StructPayload,
};
use crate::vfs::OpenFile;

use super::step_socket_create_in_namespace;

const SOCKET_FS_OBJECT_ID_BASE: u64 = 0xFFFE_0000_0000_0000;
static NEXT_SOCKET_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(SOCKET_FS_OBJECT_ID_BASE);

#[derive(Clone)]
pub struct SocketOpenFileOutput {
    pub file: Cap<OpenFile>,
    pub identity: Cap<SocketIdentity>,
    pub cloexec: bool,
    pub nonblocking: bool,
}

pub fn step_socket_open_file(
    domain: i32,
    type_: i32,
    protocol: i32,
    guard: &Guard<'_>,
) -> StepOutcome<SocketOpenFileOutput> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_socket_open_file_in_namespace(
        domain,
        type_,
        protocol,
        initial_net_namespace_payload(),
        guard,
    )
}

pub fn step_socket_open_file_in_namespace(
    domain: i32,
    type_: i32,
    protocol: i32,
    net_namespace: PayloadCap<NetNamespacePayload>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketOpenFileOutput> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let valid = match ValidSocketType::validate(domain, type_, protocol) {
        Ok(valid) => valid,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let flags = SocketHandleFlags::from_sock_flags(valid.flags);
    let identity = match step_socket_create_in_namespace(valid, net_namespace, guard) {
        StepOutcome::Done(identity) => identity,
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return StepOutcome::Err(Errno::EIO)
        }
        StepOutcome::Err(errno) => return StepOutcome::Err(errno),
    };

    match socket_open_file_from_identity(identity, flags) {
        Ok(output) => StepOutcome::Done(output),
        Err(errno) => StepOutcome::Err(errno),
    }
}

pub fn socket_open_file_from_identity(
    identity: Cap<SocketIdentity>,
    flags: SocketHandleFlags,
) -> Result<SocketOpenFileOutput, Errno> {
    let rnode = RNode::new_cap(
        allocate_socket_fs_object_id(),
        InodeMeta::new(InodeKind::Socket, 0o600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Socket {
                identity: identity.clone(),
            },
        },
    )
    .map_err(|_| Errno::ENOMEM)?;

    let file = OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: flags.cloexec,
            nonblocking: flags.nonblock,
            packet: false,
        },
    )
    .map_err(|_| Errno::ENOMEM)?;

    Ok(SocketOpenFileOutput {
        file,
        identity,
        cloexec: flags.cloexec,
        nonblocking: flags.nonblock,
    })
}

fn allocate_socket_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_SOCKET_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}
