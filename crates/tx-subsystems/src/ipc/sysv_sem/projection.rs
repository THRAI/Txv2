//! /proc/sysvipc/sem projection row.

use alloc::vec::Vec;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SysvipcSemRow {
    pub key: i32,
    pub semid: u32,
    pub mode: u16,
    pub nsems: u16,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
}

pub fn project_sysvipc_sem() -> Vec<SysvipcSemRow> {
    let mut rows: Vec<_> = crate::ipc::sysv_sem::structure::all_sem_arrays()
        .into_iter()
        .map(|array| SysvipcSemRow {
            key: array.key_raw(),
            semid: array.semid,
            mode: array.perm().mode,
            nsems: array.nsems,
            uid: array.uid(),
            gid: array.gid(),
            cuid: array.cuid,
            cgid: array.cgid,
        })
        .collect();
    rows.sort_by_key(|row| row.semid);
    rows
}
