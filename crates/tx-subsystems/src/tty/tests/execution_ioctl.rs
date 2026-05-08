//! TTY execution ioctl step tests.

use crate::execution::StepOutcome;
use crate::tty::execution::{step_ioctl_tcgets, step_ioctl_tcsets, step_read, TTY_READABLE};
use crate::tty::structure::termios::ICANON;
use crate::tty::structure::{TtyKind, TtyPayload};

use super::support::{alloc_tty, init_zones, NOOP_BINDING, TTY_ZONE_TEST_LOCK};

#[test]
fn tcsets_without_pending_canonical_bytes_leaves_read_side_quiet() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        14,
        "ttyS14",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

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
    assert_eq!(tty.input_readable.peek() & TTY_READABLE, 0);

    let mut out = [0u8; 8];
    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(0));
}
