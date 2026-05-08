// Auto-extracted from `crates/tx-subsystems/src/process/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

// POSIX `<sys/wait.h>` migration tests for
// [`crate::process::ExitStatus::wait_status_word`].

use super::*;

#[test]
fn exit_status_wait_status_word_exited_zero_encodes_zero() {
    // `(0 & 0xff) << 8 == 0` — `WIFEXITED(0) == 1`,
    // `WEXITSTATUS(0) == 0`.
    let s = ExitStatus::Exited(0).wait_status_word();
    assert_eq!(s, 0);
    // WIFEXITED predicate.
    assert_eq!(s & 0x7f, 0);
    // WEXITSTATUS extractor.
    assert_eq!((s >> 8) & 0xff, 0);
}

#[test]
fn exit_status_wait_status_word_exited_42_encodes_0x2a00() {
    // `(42 & 0xff) << 8 == 0x2a00`. WIFEXITED == 1,
    // WEXITSTATUS == 42.
    let s = ExitStatus::Exited(42).wait_status_word();
    assert_eq!(s, 0x2a00);
    assert_eq!(s & 0x7f, 0, "WIFEXITED predicate");
    assert_eq!((s >> 8) & 0xff, 42, "WEXITSTATUS extractor");
}

#[test]
fn exit_status_wait_status_word_terminated_by_sigkill_encodes_0x09() {
    // SIGKILL = 9; signaled-exit encoding = sig & 0x7f.
    let s = ExitStatus::Signaled(Signum::SIGKILL).wait_status_word();
    assert_eq!(s, 9);
    // WIFSIGNALED predicate: low 7 bits non-zero and != 0x7f.
    let low = s & 0x7f;
    assert!(low > 0 && low < 0x7f, "WIFSIGNALED predicate");
    // WTERMSIG extractor.
    assert_eq!(s & 0x7f, 9, "WTERMSIG extractor");
}
