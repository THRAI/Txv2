use super::adapter::step_engine::{self, StepOutcome as V3Out};
use super::*;

use crate::test_support::EPOCH_TEST_LOCK;
use crate::vfs::structure::{RNodeBacking, StructPayload};
use crate::zones;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn payload_of(openfile: &Cap<OpenFile>) -> Cap<PipePayload> {
    match openfile.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { payload, .. },
        } => payload.clone(),
        other => panic!("expected StructPayload::Pipe, got {other:?}"),
    }
}

fn user_page_gift_for_test() -> (crate::vm::UserPageGift, tx_hal::Ppn) {
    let aspace = crate::vm::AddressSpace::new_cap().expect("gift source aspace");
    let range = crate::vm::UserRange::new_aligned(
        crate::vm::UserVirtAddr(0x20000),
        crate::vm::USER_PAGE_SIZE,
    )
    .expect("gift source range");
    let frame =
        step_engine::page_allocator::reserve_frame(step_engine::page_allocator::ZeroPolicy::Zeroed)
            .expect("gift source frame")
            .commit();
    let ppn = frame.ppn();
    let pin = frame.try_gift_pin().expect("gift pin for source frame");
    let gift = crate::vm::UserPageGift::new_for_vm(
        ppn,
        crate::vm::UserPageGiftSource::new(aspace, range),
        pin,
        crate::vm::UserPageGiftFreeze::DetachedPrivate,
    );
    drop(frame);
    (gift, ppn)
}

#[test]
fn pipe_user_gift_push_pop_preserves_token_order() {
    let _setup = setup();
    let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let (gift, ppn) = user_page_gift_for_test();
    let guard = step_engine::guard();

    assert_eq!(
        step_push_user_page_gift(&payload, gift, &guard, false, false),
        V3Out::Done(crate::vm::USER_PAGE_SIZE)
    );

    match step_pop_user_page_gift(&payload, &guard, false) {
        V3Out::Done(Some(popped)) => assert_eq!(popped.ppn(), ppn),
        other => panic!("expected popped user gift, got {other:?}"),
    }
}

#[test]
fn pipe_user_gift_read_copies_bytes_and_releases_gift() {
    let _setup = setup();
    let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("step_pipe2");
    let payload = payload_of(&reader);
    let free_before = step_engine::page_allocator::free_count().expect("free count before gift");
    let (gift, ppn) = user_page_gift_for_test();
    let pattern = [0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe];
    step_engine::page_allocator::testing::write_frame_bytes_for_test(ppn, 96, &pattern);
    let guard = step_engine::guard();

    assert_eq!(
        step_push_user_page_gift(&payload, gift, &guard, false, false),
        V3Out::Done(crate::vm::USER_PAGE_SIZE)
    );
    let mut out = alloc::vec![0u8; crate::vm::USER_PAGE_SIZE];
    assert_eq!(
        step_read_with_post(&payload, &mut out, &guard, false, |mailbox, event| mailbox
            .post(event)),
        V3Out::Done(crate::vm::USER_PAGE_SIZE)
    );
    assert_eq!(&out[96..104], &pattern);
    assert_eq!(
        step_engine::page_allocator::free_count().expect("free count after read"),
        free_before,
        "reading an unpopped gift must drop its transfer evidence"
    );
}

#[test]
fn pipe_user_gift_tee_copies_to_anonymous_destination() {
    let _setup = setup();
    let (src_reader, _src_writer) = step_pipe2(PipeFlags::default()).expect("src pipe");
    let (dst_reader, _dst_writer) = step_pipe2(PipeFlags::default()).expect("dst pipe");
    let src = payload_of(&src_reader);
    let dst = payload_of(&dst_reader);
    let (gift, ppn) = user_page_gift_for_test();
    let pattern = [0xab, 0xcd, 0xef, 0x55];
    step_engine::page_allocator::testing::write_frame_bytes_for_test(ppn, 32, &pattern);
    let guard = step_engine::guard();

    assert_eq!(
        step_push_user_page_gift(&src, gift, &guard, false, false),
        V3Out::Done(crate::vm::USER_PAGE_SIZE)
    );
    assert_eq!(
        step_tee_to_pipe(&src, &dst, crate::vm::USER_PAGE_SIZE, &guard, false, false),
        V3Out::Done(crate::vm::USER_PAGE_SIZE)
    );

    match step_pop_user_page_gift(&dst, &guard, true) {
        V3Out::Done(None) => {}
        other => panic!("tee destination must not contain cloned linear gift, got {other:?}"),
    }

    let mut out = [0u8; 36];
    assert_eq!(
        step_read_with_post(&dst, &mut out, &guard, false, |mailbox, event| mailbox
            .post(event)),
        V3Out::Done(36)
    );
    assert_eq!(&out[32..36], &pattern);

    match step_pop_user_page_gift(&src, &guard, false) {
        V3Out::Done(Some(popped)) => assert_eq!(popped.ppn(), ppn),
        other => panic!("tee source must retain original gift, got {other:?}"),
    }
}
