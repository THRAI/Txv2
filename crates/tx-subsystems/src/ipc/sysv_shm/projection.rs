//! /proc/sysvipc/shm projection row.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SysvipcShmRow {
    pub key: i32,
    pub shmid: u32,
    pub mode: u16,
    pub size: usize,
    pub attach_count: u32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
}

pub fn project_sysvipc_shm() -> Vec<SysvipcShmRow> {
    let mut rows: Vec<_> = crate::ipc::sysv_shm::structure::all_shm_segments()
        .into_iter()
        .map(|segment| SysvipcShmRow {
            key: segment.key_raw(),
            shmid: segment.shmid,
            mode: segment.perm().mode,
            size: segment.size,
            attach_count: segment.payload.attach_count.load(Ordering::Acquire),
            uid: segment.uid(),
            gid: segment.gid(),
            cuid: segment.cuid,
            cgid: segment.cgid,
        })
        .collect();
    rows.sort_by_key(|row| row.shmid);
    rows
}
