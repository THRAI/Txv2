use super::*;
use crate::page_backed::adapter::step_engine::{
    self as step_engine, Errno as V3Errno, StepOutcome as V3Out,
};
use crate::vfs::{FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking};
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntry, VmEntryFlags,
    USER_PAGE_SIZE,
};
use alloc::vec;
use alloc::vec::Vec;
use tx_hal::UserPtr;

fn setup_host_substrate() {
    tx_test_support::init_host();
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked user-buffer tests: {error:?}"),
    }
}

fn open_file_for_pc(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(900),
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
            nonblocking: false,
        },
    )
}

/// User-space mapping fixture: an `AddressSpace` with a single anon
/// `PageContainer`-backed `VmEntry` covering `[user_va, user_va +
/// page_count * 4096)`. The `user_pc` is a kernel-side handle on the
/// same backing pages; tests that need to seed or inspect bytes at
/// the user VA do so by materialising user_pc pages and writing
/// through the kernel direct-map view.
struct UserBufferFixture {
    aspace: AddressSpace,
    user_pc: Cap<PageContainer>,
    user_va: usize,
}

impl UserBufferFixture {
    fn new(user_va: usize, page_count: u64) -> Self {
        let user_pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            page_count,
        )
        .expect("user-side page container cap");
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            UserRange::new_aligned(
                UserVirtAddr(user_va),
                (page_count as usize) * USER_PAGE_SIZE,
            )
            .expect("aligned user range"),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBacking::Page {
                pc: user_pc.clone(),
                offset: 0,
            },
        );
        let reservation = match aspace.reserve_map(entry, MapPlacement::RequireFree) {
            crate::vm::MapReserveResult::Reserved(r) => r,
            other => panic!("expected reserved, got {other:?}"),
        };
        reservation.commit().expect("commit user mapping");
        Self {
            aspace,
            user_pc,
            user_va,
        }
    }

    /// Seed the user-side PC with `bytes` starting at user VA.
    fn seed_user_bytes(&self, bytes: &[u8]) {
        write_into_pc(&self.user_pc, 0, bytes);
    }

    /// Read `len` bytes from the user-side PC at offset 0.
    fn read_user_bytes(&self, len: usize) -> Vec<u8> {
        read_from_pc(&self.user_pc, 0, len)
    }

    fn user_ptr(&self) -> UserPtr<u8> {
        UserPtr::<u8>::new(self.user_va)
    }
}

fn write_into_pc(pc: &PageContainer, byte_offset: usize, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
        let cursor = byte_offset + written;
        let page_index = PageIndex::new((cursor / USER_PAGE_SIZE) as u64);
        let within = cursor % USER_PAGE_SIZE;
        let chunk = core::cmp::min(bytes.len() - written, USER_PAGE_SIZE - within);
        let page = pc
            .materialize_anon(page_index, MaterializeAccess::Write)
            .expect("materialise user-side page for seeding");
        let frame_base = page_allocator::frame_kernel_addr(page.ppn)
            .expect("kernel direct-map for materialised page");
        // SAFETY: frame_base is a valid kernel direct-map pointer to
        // the materialised page; within + chunk <= USER_PAGE_SIZE; the
        // source slice has at least `chunk` bytes remaining.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(written),
                frame_base.add(within),
                chunk,
            );
        }
        written += chunk;
    }
}

fn read_from_pc(pc: &PageContainer, byte_offset: usize, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut read = 0usize;
    while read < len {
        let cursor = byte_offset + read;
        let page_index = PageIndex::new((cursor / USER_PAGE_SIZE) as u64);
        let within = cursor % USER_PAGE_SIZE;
        let chunk = core::cmp::min(len - read, USER_PAGE_SIZE - within);
        let page = pc
            .materialize_anon(page_index, MaterializeAccess::Read)
            .expect("materialise user-side page for read-back");
        let frame_base = page_allocator::frame_kernel_addr(page.ppn)
            .expect("kernel direct-map for materialised page");
        // SAFETY: frame_base is a valid kernel direct-map pointer to
        // the materialised page; within + chunk <= USER_PAGE_SIZE.
        unsafe {
            core::ptr::copy_nonoverlapping(
                frame_base.add(within),
                out.as_mut_ptr().add(read),
                chunk,
            );
        }
        read += chunk;
    }
    out
}

#[test]
fn pagebacked_round_trip_through_user_buffer_preserves_bytes_in_one_page() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let fixture = UserBufferFixture::new(0x10_0000, 1);
    let guard = step_engine::guard();

    let payload: Vec<u8> = (0u8..200).collect();
    fixture.seed_user_bytes(&payload);
    let writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &pc,
        &writer,
        &fixture.aspace,
        fixture.user_ptr(),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert_eq!(writer.offset(), payload.len() as u64);

    // Clear the user buffer to prove the read genuinely re-fills it.
    let zeros = vec![0u8; payload.len()];
    fixture.seed_user_bytes(&zeros);
    let reader = open_file_for_pc(&pc);
    let outcome = step_read_to_user(
        &pc,
        &reader,
        &fixture.aspace,
        fixture.user_ptr(),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert_eq!(reader.offset(), payload.len() as u64);
    assert_eq!(fixture.read_user_bytes(payload.len()), payload);
}

#[test]
fn pagebacked_round_trip_through_user_buffer_crosses_page_boundary() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let fixture = UserBufferFixture::new(0x20_0000, 2);
    let guard = step_engine::guard();

    let payload: Vec<u8> = (0..(USER_PAGE_SIZE + 23))
        .map(|i| (i & 0xff) as u8)
        .collect();
    fixture.seed_user_bytes(&payload);
    let writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &pc,
        &writer,
        &fixture.aspace,
        fixture.user_ptr(),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);

    let zeros = vec![0u8; payload.len()];
    fixture.seed_user_bytes(&zeros);
    let reader = open_file_for_pc(&pc);
    let outcome = step_read_to_user(
        &pc,
        &reader,
        &fixture.aspace,
        fixture.user_ptr(),
        payload.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(payload.len()));
    assert_eq!(fixture.read_user_bytes(payload.len()), payload);
}

#[test]
fn pagebacked_step_read_to_user_efault_propagates_when_user_va_unmapped() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    // PC has bytes; aspace does NOT cover user_va — copy_to_user will EFAULT.
    let payload: Vec<u8> = (0u8..32).collect();
    let writer_fixture = UserBufferFixture::new(0x30_0000, 1);
    let empty_aspace = AddressSpace::new();
    let guard = step_engine::guard();
    writer_fixture.seed_user_bytes(&payload);
    let writer = open_file_for_pc(&pc);
    assert_eq!(
        step_write_from_user(
            &pc,
            &writer,
            &writer_fixture.aspace,
            writer_fixture.user_ptr(),
            payload.len(),
            &guard,
        ),
        V3Out::Done(payload.len())
    );

    // Use a fresh aspace with NO recipe and a dangling user VA.
    let dangling = UserPtr::<u8>::new(0x40_0000);

    let reader = open_file_for_pc(&pc);
    let outcome = step_read_to_user(&pc, &reader, &empty_aspace, dangling, 16, &guard);
    assert_eq!(outcome, V3Out::Err(V3Errno::EFAULT));
    assert_eq!(reader.offset(), 0);
}

#[test]
fn pagebacked_truncate_shrink_then_grow_reads_zeros_for_post_eof_region() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let fixture = UserBufferFixture::new(0x50_0000, 2);
    let guard = step_engine::guard();

    let pattern: Vec<u8> = (0..(USER_PAGE_SIZE + 32))
        .map(|i| ((i & 0xff) | 0x20) as u8)
        .collect();
    fixture.seed_user_bytes(&pattern);
    let writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(
        &pc,
        &writer,
        &fixture.aspace,
        fixture.user_ptr(),
        pattern.len(),
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(pattern.len()));

    let shrink_size = USER_PAGE_SIZE as u64 + 4;
    assert_eq!(step_truncate(&pc, shrink_size, &guard), V3Out::Done(()));

    let grow_size = USER_PAGE_SIZE as u64 + 32;
    assert_eq!(step_truncate(&pc, grow_size, &guard), V3Out::Done(()));

    let zeros = vec![0u8; pattern.len()];
    fixture.seed_user_bytes(&zeros);
    let reader = open_file_for_pc(&pc);
    reader.set_offset(USER_PAGE_SIZE as u64);
    let outcome = step_read_to_user(
        &pc,
        &reader,
        &fixture.aspace,
        fixture.user_ptr(),
        32,
        &guard,
    );
    assert_eq!(outcome, V3Out::Done(32));
    let received = fixture.read_user_bytes(32);
    assert_eq!(&received[..4], &pattern[USER_PAGE_SIZE..USER_PAGE_SIZE + 4]);
    assert!(
        received[4..].iter().all(|b| *b == 0),
        "post-EOF region must read as zeros after shrink-then-grow, got {:?}",
        &received[4..]
    );
}

#[test]
fn pagebacked_step_write_from_user_propagates_efault_without_advance() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed user-buffer test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    // No recipe in this aspace → user VA dereference yields EFAULT.
    let empty_aspace = AddressSpace::new();
    let guard = step_engine::guard();
    let dangling = UserPtr::<u8>::new(0x60_0000);
    let writer = open_file_for_pc(&pc);
    let outcome = step_write_from_user(&pc, &writer, &empty_aspace, dangling, 8, &guard);
    assert_eq!(outcome, V3Out::Err(V3Errno::EFAULT));
    assert_eq!(writer.offset(), 0);
    assert_eq!(pc.size_bytes(), pc.page_count() * USER_PAGE_SIZE as u64);
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave-3 smoke tests for `ReadToUserOp`/`WriteFromUserOp`
    //! `StepOp` wraps.
    //!
    //! Each test builds the `*Op` adapter, drives it through a single
    //! `.step(&mut ctx)` call, and pins the outcome variant against the
    //! same expectation as the free-fn suite above. Compile-checks
    //! `impl StepOp` correctness; the heavy-lifting semantics tests
    //! live in the free-fn suite.
    use super::*;
    use crate::page_backed::{ReadToUserOp, WriteFromUserOp};
    use step_engine::{PlaceholderProcessSubject, ScriptCtx, StepOp};

    #[test]
    fn write_from_user_op_round_trip_one_page() {
        let _lock = EPOCH_TEST_LOCK
            .lock()
            .expect("page-backed user-buffer step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            2,
        );
        let fixture = UserBufferFixture::new(0x70_0000, 1);
        let guard = step_engine::guard();

        let payload: Vec<u8> = (0u8..200).collect();
        fixture.seed_user_bytes(&payload);
        let writer = open_file_for_pc(&pc);
        drop(guard);
        let mut op = WriteFromUserOp {
            pc: &pc,
            of: &writer,
            aspace: &fixture.aspace,
            src: fixture.user_ptr(),
            len: payload.len(),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(payload.len()));
        assert_eq!(writer.offset(), payload.len() as u64);
        assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    }

    #[test]
    fn read_to_user_op_after_seed_returns_done_with_bytes() {
        let _lock = EPOCH_TEST_LOCK
            .lock()
            .expect("page-backed user-buffer step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            2,
        );
        let fixture = UserBufferFixture::new(0x80_0000, 1);
        let guard = step_engine::guard();

        let payload: Vec<u8> = (0u8..96).collect();
        fixture.seed_user_bytes(&payload);
        // First write the bytes into pc via free fn so size grows.
        let writer = open_file_for_pc(&pc);
        assert_eq!(
            step_write_from_user(
                &pc,
                &writer,
                &fixture.aspace,
                fixture.user_ptr(),
                payload.len(),
                &guard,
            ),
            V3Out::Done(payload.len())
        );

        // Clear the user buffer and read back through the wrap.
        let zeros = vec![0u8; payload.len()];
        fixture.seed_user_bytes(&zeros);
        let reader = open_file_for_pc(&pc);
        drop(guard);
        let mut op = ReadToUserOp {
            pc: &pc,
            of: &reader,
            aspace: &fixture.aspace,
            dst: fixture.user_ptr(),
            len: payload.len(),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(payload.len()));
        assert_eq!(reader.offset(), payload.len() as u64);
        assert_eq!(fixture.read_user_bytes(payload.len()), payload);
    }

    #[test]
    fn read_to_user_op_zero_len_returns_done_zero() {
        let _lock = EPOCH_TEST_LOCK
            .lock()
            .expect("page-backed user-buffer step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        let fixture = UserBufferFixture::new(0x90_0000, 1);
        let guard = step_engine::guard();
        let reader = open_file_for_pc(&pc);
        drop(guard);
        let mut op = ReadToUserOp {
            pc: &pc,
            of: &reader,
            aspace: &fixture.aspace,
            dst: fixture.user_ptr(),
            len: 0,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Done(0));
        assert_eq!(reader.offset(), 0);
    }

    #[test]
    fn write_from_user_op_efault_propagates_without_advance() {
        let _lock = EPOCH_TEST_LOCK
            .lock()
            .expect("page-backed user-buffer step_op_wraps lock");
        setup_host_substrate();
        let pc = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        let empty_aspace = AddressSpace::new();
        let guard = step_engine::guard();
        let dangling = UserPtr::<u8>::new(0xA0_0000);
        let writer = open_file_for_pc(&pc);
        drop(guard);
        let mut op = WriteFromUserOp {
            pc: &pc,
            of: &writer,
            aspace: &empty_aspace,
            src: dangling,
            len: 8,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3Out::Err(V3Errno::EFAULT));
        assert_eq!(writer.offset(), 0);
    }
}
