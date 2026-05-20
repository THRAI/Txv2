/// Bootstrap fixtures for pthread/libctest testing.
use tx_subsystems::vfs::{Credential, RNodeBacking};
use tx_subsystems::page_backed::{PageIndex, MaterializeAccess};
use tx_subsystems::vm::USER_PAGE_SIZE;

const TEST_BYTES: &[u8] = include_bytes!("pthread_test_fixture.bin");

pub(crate) fn register_pthread_test_into_tmpfs<P: tx_hal::TxPlatform>() {
    use crate::adapter::step_engine::{self as step_engine, page_allocator, StepOutcome};
    use crate::init::root_mount;
    use StepOutcome as V3;

    let root_mount = match root_mount() {
        Some(m) => m,
        None => return,
    };
    let payload = match root_mount.payload_cap() {
        Ok(p) => p.into_cap(),
        Err(_) => return,
    };
    let fs_ops = payload.fs_ops.clone();
    let fs_page_backing = payload.fs_page_backing.clone();
    let root_id = root_mount.root().fs_object_id();
    let cred = Credential::root();

    let bytes = TEST_BYTES;
    let (file_id, file_meta) = {
        let guard = step_engine::guard();
        match fs_ops.create_inode(root_id, b"pthread_test", 0o100755, &cred, &guard) {
            V3::Done(out) => out,
            V3::Err(step_engine::Errno::EROFS)
            | V3::Err(step_engine::Errno::ENOSYS)
            | V3::Err(step_engine::Errno::EEXIST) => {
                crate::init::CoreInit::<P>::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":pthread:fixture:skip\n");
                return;
            }
            other => panic!("pthread fixture: create_inode: {other:?}"),
        }
    };

    let pc = {
        let guard = step_engine::guard();
        match fs_ops.materialise_rnode(file_id, file_meta, &payload, &guard) {
            V3::Done(rnode) => match rnode.backing() {
                RNodeBacking::PageBacked { pc } => pc.clone(),
                other => panic!("non-PageBacked backing: {other:?}"),
            },
            other => panic!("materialise_rnode: {other:?}"),
        }
    };

    let page_size = USER_PAGE_SIZE;
    for (idx, chunk) in bytes.chunks(page_size).enumerate() {
        let mat = pc
            .materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write)
            .expect("materialize_anon");
        let base = page_allocator::frame_kernel_addr(mat.ppn)
            .expect("frame_kernel_addr");
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), base, chunk.len()); }
    }

    {
        let guard = step_engine::guard();
        let _ = fs_page_backing.truncate(file_id, bytes.len() as u64, &guard);
    }

    // Create root-level `init` symlink → `pthread_test`.
    {
        let guard = step_engine::guard();
        match fs_ops.symlink(root_id, b"init", b"pthread_test", &cred, &guard) {
            V3::Done((_id, _meta)) => {}
            V3::Err(step_engine::Errno::EEXIST) => {}
            other => panic!("symlink(/init -> pthread_test): {other:?}"),
        }
    }

    crate::init::CoreInit::<P>::write_board_sentinel_prefix();
    tx_hal::console_write_str::<P>(":pthread:fixture:ok\n");
}
