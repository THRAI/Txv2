//! Compatibility placeholder.
//!
//! The old AIO e2e tests targeted Tx's temporary fd-shaped
//! `aio_context_t` contract. Raw Linux ABI e2e coverage now lives in
//! `v3_aio_raw_abi.rs`.

#[test]
fn aio_e2e_coverage_moved_to_raw_abi_suite() {
    // Kept so historical targeted commands still find a test binary.
}
