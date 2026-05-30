//! Compatibility placeholder.
//!
//! The old `v3_aio_io_setup` suite pinned Tx's temporary fd-shaped
//! `aio_context_t` contract. Raw Linux ABI coverage now lives in
//! `v3_aio_raw_abi.rs`.

#[test]
fn aio_io_setup_coverage_moved_to_raw_abi_suite() {
    // Kept so historical targeted commands still find a test binary.
}
