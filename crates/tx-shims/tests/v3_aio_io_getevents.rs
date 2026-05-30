//! Compatibility placeholder.
//!
//! The old getevents tests targeted the kernel-side completion queue
//! behind fd-shaped AIO handles. Raw Linux ring coverage now lives in
//! `v3_aio_raw_abi.rs`.

#[test]
fn aio_io_getevents_coverage_moved_to_raw_abi_suite() {
    // Kept so historical targeted commands still find a test binary.
}
