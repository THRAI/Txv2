//! /proc/sysvipc/msg projection row.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SysvipcMsgRow {
    pub key: i32,
    pub msqid: u32,
    pub mode: u16,
    pub current_bytes: u64,
    pub msg_count: u32,
    pub qbytes: usize,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
}

pub fn project_sysvipc_msg() -> Vec<SysvipcMsgRow> {
    let mut rows: Vec<_> = crate::ipc::sysv_msg::structure::all_msg_queues()
        .into_iter()
        .filter_map(|queue| {
            let payload_guard = queue.payload.lock();
            let payload = payload_guard.as_ref()?;
            Some(SysvipcMsgRow {
                key: queue.key_raw(),
                msqid: queue.msqid,
                mode: queue.perm().mode,
                current_bytes: payload.current_bytes.load(Ordering::Acquire),
                msg_count: payload.msg_count.load(Ordering::Acquire),
                qbytes: payload.max_bytes,
                uid: queue.uid(),
                gid: queue.gid(),
                cuid: queue.cuid,
                cgid: queue.cgid,
            })
        })
        .collect();
    rows.sort_by_key(|row| row.msqid);
    rows
}
