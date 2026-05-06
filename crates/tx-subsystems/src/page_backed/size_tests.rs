use super::*;
use crate::execution::{Errno, StepOutcome};
use crate::vfs::{FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking};

fn setup_host_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked size tests: {error:?}"),
    }
}

fn open_file_for_pc(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(700),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode cap");
    OpenFile::new(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
        },
    )
}

#[test]
fn page_container_size_starts_at_fixed_capacity() {
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        3,
    );

    assert_eq!(pc.size_bytes(), 3 * crate::vm::USER_PAGE_SIZE as u64);
}

#[test]
fn pagebacked_step_read_uses_visible_size_not_capacity() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed size test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
        StepOutcome::Done(())
    );
    let mut of = open_file_for_pc(&pc);
    of.set_offset(crate::vm::USER_PAGE_SIZE as u64);

    assert_eq!(step_read(&pc, &mut of, 16, &guard), StepOutcome::Done(0));
    assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_write_extends_visible_size_within_capacity() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed size test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    assert_eq!(step_truncate(&pc, 8, &guard), StepOutcome::Done(()));
    let mut of = open_file_for_pc(&pc);
    of.set_offset((crate::vm::USER_PAGE_SIZE + 9) as u64);

    assert_eq!(step_write(&pc, &mut of, 7, &guard), StepOutcome::Done(7));

    assert_eq!(of.offset(), (crate::vm::USER_PAGE_SIZE + 16) as u64);
    assert_eq!(pc.size_bytes(), (crate::vm::USER_PAGE_SIZE + 16) as u64);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
}

#[test]
fn pagebacked_step_write_rejects_growth_beyond_capacity_without_size_change() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("page-backed size test lock");
    setup_host_substrate();
    let guard = tx_substrate::epoch::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    assert_eq!(step_truncate(&pc, 8, &guard), StepOutcome::Done(()));
    let mut of = open_file_for_pc(&pc);
    of.set_offset(crate::vm::USER_PAGE_SIZE as u64 - 4);

    assert_eq!(
        step_write(&pc, &mut of, 8, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );

    assert_eq!(of.offset(), crate::vm::USER_PAGE_SIZE as u64 - 4);
    assert_eq!(pc.size_bytes(), 8);
    assert_eq!(pc.resident_pages(), 0);
}
