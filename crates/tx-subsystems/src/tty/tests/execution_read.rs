//! TTY execution read step tests.

use crate::execution::StepOutcome;
use crate::tty::execution::{step_ingest, step_ioctl_tcgets, step_ioctl_tcsets, step_read};
use crate::tty::structure::termios::ICANON;
use crate::tty::structure::{TtyKind, TtyPayload};

use super::support::{alloc_tty, init_zones, NOOP_BINDING, TTY_ZONE_TEST_LOCK};

#[test]
fn noncanonical_vmin_blocks_until_threshold_is_met() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        12,
        "ttyS12",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let mut out = [0u8; 8];

    let mut raw_mode = match step_ioctl_tcgets(&tty, &guard) {
        StepOutcome::Done(termios) => termios,
        other => panic!("tcgets failed: {other:?}"),
    };
    raw_mode.c_lflag &= !ICANON;
    raw_mode.c_cc[crate::tty::structure::termios::VMIN] = 3;
    raw_mode.c_cc[crate::tty::structure::termios::VTIME] = 0;
    assert_eq!(
        step_ioctl_tcsets(&tty, raw_mode, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect::default())
    );

    assert_eq!(
        step_ingest(&tty, b"xy", &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 2,
            readable_fired: true,
            writable_fired: true,
            ..Default::default()
        })
    );
    assert!(matches!(
        step_read(&tty, &mut out, &guard),
        StepOutcome::Blocked(_)
    ));

    assert_eq!(
        step_ingest(&tty, b"z", &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: true,
            writable_fired: true,
            ..Default::default()
        })
    );
    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(3));
    assert_eq!(&out[..3], b"xyz");
}

#[test]
fn noncanonical_vmin_zero_allows_empty_read() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        13,
        "ttyS13",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let mut out = [0u8; 8];

    let mut raw_mode = match step_ioctl_tcgets(&tty, &guard) {
        StepOutcome::Done(termios) => termios,
        other => panic!("tcgets failed: {other:?}"),
    };
    raw_mode.c_lflag &= !ICANON;
    raw_mode.c_cc[crate::tty::structure::termios::VMIN] = 0;
    raw_mode.c_cc[crate::tty::structure::termios::VTIME] = 0;
    assert_eq!(
        step_ioctl_tcsets(&tty, raw_mode, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect::default())
    );

    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(0));
}
