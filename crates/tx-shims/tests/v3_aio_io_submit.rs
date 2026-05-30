//! Compatibility placeholder.
//!
//! The old submit tests targeted fd-shaped AIO handles. Raw Linux ABI
//! submit coverage now lives in `v3_aio_raw_abi.rs`.

#[test]
fn aio_io_submit_coverage_moved_to_raw_abi_suite() {
    // Kept so historical targeted commands still find a test binary.
}
