//! PR-11 follow-up — OpenFile::step_read PageBacked path (W-KK
//! 2026-05-12). Pins the `RNodeBacking::PageBacked` arm of
//! `OpenFile::step_read` and `OpenFile::step_write` independently of
//! the AIO worker / dispatcher: a direct `step_read` / `step_write` on
//! a page-backed `OpenFile` must move bytes through the page-cache
//! materialiser, advance `of.offset()`, and short-read at EOF.
//!
//! W-JJ's gap analysis (2026-05-12 STATUS catchup) noted that
//! `OpenFile::step_read` returned `ENOSYS` for `RNodeBacking::PageBacked`
//! — the AIO PREAD canary's `event.res` was therefore `-ENOSYS` until
//! the page-backed-read seam landed. W-KK closed the seam by routing
//! the PageBacked arm of both `step_read` and `step_write` through new
//! kernel-buffer helpers `page_backed::step_read_to_kernel` /
//! `step_write_from_kernel` (kernel-buffer cousins to
//! `step_read_to_user` / `step_write_from_user`). This integration test
//! pins that path:
//!
//! 1. **read-against-fresh-page-backed-file-returns-zeroes**. A
//!    freshly-constructed `PageContainer` of 1 page has `size_bytes`
//!    seeded to capacity (4096); a `step_read` of 32 bytes at offset 0
//!    returns `Done(32)` of zero bytes (the materialised page is
//!    zero-filled).
//! 2. **write-then-read-round-trips-bytes**. `step_write` of a known
//!    pattern advances `of.offset()` by the pattern length and grows
//!    `PC.size` if it was below; a subsequent `step_read` at offset 0
//!    returns the same bytes.
//! 3. **read-at-eof-short-reads**. A `step_read` whose offset is at
//!    `PC.size_bytes()` returns `Done(0)`; a `step_read` that starts
//!    inside the visible region but spans past EOF returns `Done(n)`
//!    with `n` = bytes-up-to-EOF.
//! 4. **read-flag-off-returns-einval**. `OpenFileFlags::read = false`
//!    forces `step_read` to return `Err(EINVAL)` before any backing
//!    dispatch.
//! 5. **write-flag-off-returns-einval**. Symmetric for `step_write`.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use tx_subsystems::page_backed::adapter::step_engine::{
    self as zone, guard as ebr_guard, page_allocator, Errno as V3Errno, StepOutcome as V3Out,
};

use tx_subsystems::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::zones;

fn setup_substrate() {
    tx_test_support::init_host();
    let _ = zones::register_all();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame: {error:?}"),
    }
    tx_test_support::drain_to_quiescence();
}

fn make_page_backed_open_file(page_count: u64, read: bool, write: bool) -> zone::Cap<OpenFile> {
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
    .expect("page container cap");
    let raw = RNode::new(
        FsObjectId::new(0xF11E),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    );
    let res = zone::reserve_for::<RNode>().expect("rnode reservation");
    let rnode = zone::sign_for(res, raw);
    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read,
            write,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("open file cap")
}

/// Single integration test that bundles every PageBacked-read /
/// PageBacked-write invariant (the `reset_*_for_test` helpers needed
/// to split this into multiple tests are not visible from integration
/// test binaries; the bundled form mirrors the
/// `v3_cred_zone_allocation` / `v3_vfs_waitsource` precedent).
#[test]
fn openfile_step_read_page_backed_round_trips() {
    setup_substrate();

    // (1) Read against a fresh 1-page file returns 32 zero bytes.
    {
        let file = make_page_backed_open_file(/* page_count */ 1, true, true);
        let mut buf = vec![0xAA_u8; 32];
        let guard = ebr_guard();
        match file.step_read(&mut buf, &guard) {
            V3Out::Done(n) => {
                assert_eq!(n, 32, "fresh 1-page file: step_read returns 32 bytes");
                assert!(
                    buf.iter().all(|b| *b == 0),
                    "fresh page is zero-filled, buf = {buf:?}"
                );
            }
            other => panic!("fresh page step_read: unexpected {other:?}"),
        }
        // offset advanced by 32.
        assert_eq!(file.offset(), 32);
    }

    // (2) Write-then-read round-trip preserves bytes.
    {
        let file = make_page_backed_open_file(1, true, true);
        let pattern: Vec<u8> = (0u8..64).collect();
        let guard = ebr_guard();
        match file.step_write(&pattern, &guard) {
            V3Out::Done(n) => assert_eq!(n, 64, "step_write returns 64 bytes"),
            other => panic!("step_write: unexpected {other:?}"),
        }
        assert_eq!(file.offset(), 64);
        // Rewind and read back through a fresh OpenFile on the same
        // PageContainer to keep offset semantics independent.
        file.set_offset(0);
        let mut readback = vec![0u8; 64];
        match file.step_read(&mut readback, &guard) {
            V3Out::Done(n) => assert_eq!(n, 64, "step_read returns 64 bytes"),
            other => panic!("step_read after write: unexpected {other:?}"),
        }
        assert_eq!(readback, pattern, "write-then-read byte-equality");
    }

    // (3) Short-read at EOF.
    {
        let file = make_page_backed_open_file(1, true, true);
        // Seek past the visible size — read returns Done(0).
        file.set_offset(8192); // > page_count * USER_PAGE_SIZE = 4096
        let mut buf = vec![0xCC_u8; 16];
        let guard = ebr_guard();
        match file.step_read(&mut buf, &guard) {
            V3Out::Done(n) => assert_eq!(n, 0, "read past EOF returns 0"),
            other => panic!("read past EOF: unexpected {other:?}"),
        }
        // Offset clamping is left to step_lseek; step_read itself just
        // observes the offset.
        assert_eq!(buf[0], 0xCC, "buf untouched on Done(0)");

        // Position just before EOF: read of more than remaining
        // short-reads at EOF.
        file.set_offset(4080); // remaining = 16 bytes
        let mut buf = vec![0xCC_u8; 64];
        match file.step_read(&mut buf, &guard) {
            V3Out::Done(n) => {
                assert_eq!(n, 16, "short read at EOF returns remaining bytes");
            }
            other => panic!("short read at EOF: unexpected {other:?}"),
        }
        assert_eq!(file.offset(), 4096, "offset advanced to EOF");
    }

    // (4) read-flag-off returns EINVAL before any backing dispatch.
    {
        let file = make_page_backed_open_file(1, /* read */ false, /* write */ true);
        let mut buf = vec![0u8; 16];
        let guard = ebr_guard();
        match file.step_read(&mut buf, &guard) {
            V3Out::Err(V3Errno::EINVAL) => {}
            other => panic!("read with read=false: expected Err(EINVAL), got {other:?}"),
        }
    }

    // (5) write-flag-off returns EINVAL before any backing dispatch.
    {
        let file = make_page_backed_open_file(1, /* read */ true, /* write */ false);
        let buf = [0u8; 16];
        let guard = ebr_guard();
        match file.step_write(&buf, &guard) {
            V3Out::Err(V3Errno::EINVAL) => {}
            other => panic!("write with write=false: expected Err(EINVAL), got {other:?}"),
        }
    }

    // Final housekeeping.
    tx_test_support::drain_to_quiescence();
}
