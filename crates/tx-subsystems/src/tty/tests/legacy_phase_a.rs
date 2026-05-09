//! Phase A verification tests for the TTY line discipline.
//!
//! Covers the required cases from TTY_DESIGN_PLAN.md §A.8.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Errno, Guard, StepOutcome};
use crate::test_support::EPOCH_TEST_LOCK as TTY_ZONE_TEST_LOCK;
use crate::tty::execution::{
    register_console_alias, register_hardware, step_hangup, step_ingest, step_ioctl_tcgets,
    step_ioctl_tcsets, step_ioctl_tiocgpgrp, step_ioctl_tiocgwinsz, step_ioctl_tiocnotty,
    step_ioctl_tiocsctty, step_ioctl_tiocspgrp, step_ioctl_tiocswinsz, step_master_close_last,
    step_poll_hardware_input, step_read, step_read_for_caller, step_write, step_write_for_caller,
    IoctlCaller, JobControlSignal, SignalDispatch, SignalTarget, TTY_READABLE,
};
use crate::tty::ldisc::state::LdiscState;
use crate::tty::ldisc::{
    on_termios_changed, process_input_byte, process_output, FlowCtl, LdiscInputEffect, SignalKind,
};
use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::{Termios, ICANON, IXON, TOSTOP};
use crate::tty::structure::{SessionPgrp, TtyIdentity, TtyKind, TtyPayload, Winsize};
use crate::vfs::{Credential, DirCursor, FsOps, InodeKind};

struct NoopOps;

impl CharDeviceOps for NoopOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(bytes.len())
    }
}

static NOOP_OPS: NoopOps = NoopOps;
static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "tty-test",
    ops: &NOOP_OPS,
};

struct ScriptedReadOps {
    script: std::sync::Mutex<Vec<u8>>,
    writes: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl ScriptedReadOps {
    fn new(bytes: &[u8]) -> Self {
        Self {
            script: std::sync::Mutex::new(bytes.to_vec()),
            writes: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn recorded_writes(&self) -> Vec<Vec<u8>> {
        self.writes.lock().expect("writes lock").clone()
    }
}

impl CharDeviceOps for ScriptedReadOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        let mut script = self.script.lock().expect("script lock");
        let copied = out.len().min(script.len());
        out[..copied].copy_from_slice(&script[..copied]);
        script.drain(..copied);
        StepOutcome::Done(copied)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        self.writes
            .lock()
            .expect("writes lock")
            .push(bytes.to_vec());
        StepOutcome::Done(bytes.len())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const CAP: usize = 512;
type Ring = TtyRing<CAP>;

fn cooked() -> Termios {
    Termios::default_cooked()
}

fn raw() -> Termios {
    let mut t = Termios::default_cooked();
    t.c_lflag &= !ICANON;
    t
}

/// Feed a byte slice through the ldisc and return the effect of the last byte.
fn feed(
    state: &mut LdiscState,
    termios: &Termios,
    iq: &mut Ring,
    oq: &mut Ring,
    bytes: &[u8],
) -> LdiscInputEffect {
    let mut last = LdiscInputEffect::Absorbed;
    for &b in bytes {
        last = process_input_byte(state, termios, iq, oq, b);
    }
    last
}

/// Drain all bytes from a ring into a Vec.
fn drain(ring: &mut Ring) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(b) = ring.pop() {
        out.push(b);
    }
    out
}

fn init_zones() {
    tx_substrate::testing::init_host_for_test_once();
    crate::zones::register_all().expect("kernel zones");
    crate::tty::structure::registry::reset_for_tests();
}

fn alloc_tty(kind: TtyKind, index: u32, name: &str, payload: TtyPayload) -> Cap<TtyIdentity> {
    let id_res = zone::reserve_for::<TtyIdentity>().expect("tty identity reservation");
    let payload_res = zone::reserve_for::<TtyPayload>().expect("tty payload reservation");
    let payload = PayloadCap::from_cap(zone::sign_for(payload_res, payload));
    let identity = zone::sign_for(id_res, TtyIdentity::new(kind, index, name));
    identity.install_payload(payload);
    identity
}

// ---------------------------------------------------------------------------
// Canonical mode input
// ---------------------------------------------------------------------------

#[test]
fn cooked_not_readable_before_newline() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // 'a', 'b', 'c' must all be Absorbed (no full line yet).
    for &b in b"abc" {
        let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, b);
        assert_eq!(eff, LdiscInputEffect::Absorbed);
    }
    assert!(iq.is_empty(), "input queue must be empty before newline");
}

#[test]
fn cooked_line_committed_on_newline() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    feed(&mut st, &t, &mut iq, &mut oq, b"abc");
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, b'\n');
    assert_eq!(eff, LdiscInputEffect::LineCommitted);
    assert_eq!(drain(&mut iq), b"abc\n");
}

// ---------------------------------------------------------------------------
// Raw (non-canonical) mode
// ---------------------------------------------------------------------------

#[test]
fn raw_byte_immediately_readable() {
    let mut st = LdiscState::new();
    let t = raw();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, b'a');
    assert_eq!(eff, LdiscInputEffect::QueuedForRead);
    assert_eq!(drain(&mut iq), b"a");
}

// ---------------------------------------------------------------------------
// Editing control characters
// ---------------------------------------------------------------------------

#[test]
fn verase_deletes_last_byte() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    feed(&mut st, &t, &mut iq, &mut oq, b"ab");
    // VERASE = DEL = 0x7f
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x7f);
    assert_eq!(eff, LdiscInputEffect::Absorbed);

    // Only 'a' should remain; commit with '\n'.
    process_input_byte(&mut st, &t, &mut iq, &mut oq, b'\n');
    assert_eq!(drain(&mut iq), b"a\n");
}

#[test]
fn verase_on_empty_buffer_is_noop() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x7f);
    assert_eq!(eff, LdiscInputEffect::Absorbed);
    assert!(iq.is_empty());
}

#[test]
fn vkill_clears_cooked_buffer() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    feed(&mut st, &t, &mut iq, &mut oq, b"hello");
    // VKILL = ^U = 0x15
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x15);
    assert_eq!(eff, LdiscInputEffect::Absorbed);

    // Buffer should be empty; committing '\n' produces only "\n".
    process_input_byte(&mut st, &t, &mut iq, &mut oq, b'\n');
    assert_eq!(drain(&mut iq), b"\n");
}

// ---------------------------------------------------------------------------
// Signal generation
// ---------------------------------------------------------------------------

#[test]
fn vintr_produces_sigint() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // VINTR = ^C = 0x03
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x03);
    assert_eq!(eff, LdiscInputEffect::SignalFgPgrp(SignalKind::Int));
    assert!(
        iq.is_empty(),
        "SIGINT must not deposit bytes in input_queue"
    );
}

#[test]
fn vquit_produces_sigquit() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // VQUIT = ^\ = 0x1c
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x1c);
    assert_eq!(eff, LdiscInputEffect::SignalFgPgrp(SignalKind::Quit));
}

#[test]
fn vsusp_produces_sigtstp() {
    let mut st = LdiscState::new();
    let t = cooked();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // VSUSP = ^Z = 0x1a
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x1a);
    assert_eq!(eff, LdiscInputEffect::SignalFgPgrp(SignalKind::Tstp));
}

#[test]
fn signal_disabled_by_posix_vdisable() {
    let mut st = LdiscState::new();
    let mut t = cooked();
    // Disable VINTR by setting it to POSIX_VDISABLE (0xff).
    t.c_cc[crate::tty::structure::termios::VINTR] = 0xff;
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // 0x03 should now be treated as a normal character, not SIGINT.
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x03);
    assert_ne!(eff, LdiscInputEffect::SignalFgPgrp(SignalKind::Int));
}

#[test]
fn isig_off_suppresses_signal() {
    let mut st = LdiscState::new();
    let mut t = cooked();
    t.c_lflag &= !crate::tty::structure::termios::ISIG;
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // With ISIG off, ^C should be treated as a regular canonical character.
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x03);
    assert_ne!(eff, LdiscInputEffect::SignalFgPgrp(SignalKind::Int));
}

// ---------------------------------------------------------------------------
// Flow control
// ---------------------------------------------------------------------------

#[test]
fn vstop_stops_output_and_vstart_resumes() {
    let mut st = LdiscState::new();
    let t = cooked(); // IXON is set in default_cooked
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // VSTOP = ^S = 0x13
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x13);
    assert_eq!(eff, LdiscInputEffect::FlowControl(FlowCtl::Stop));
    assert!(st.flow_stopped);

    // VSTART = ^Q = 0x11
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x11);
    assert_eq!(eff, LdiscInputEffect::FlowControl(FlowCtl::Start));
    assert!(!st.flow_stopped);
}

#[test]
fn byte_absorbed_while_flow_stopped_without_ixany() {
    let mut st = LdiscState::new();
    let mut t = cooked();
    // Clear IXANY so only VSTART can resume.
    t.c_iflag &= !crate::tty::structure::termios::IXANY;
    // Ensure IXON is set.
    t.c_iflag |= IXON;
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // Stop output.
    process_input_byte(&mut st, &t, &mut iq, &mut oq, 0x13);
    assert!(st.flow_stopped);

    // Any non-VSTART byte should be Absorbed.
    let eff = process_input_byte(&mut st, &t, &mut iq, &mut oq, b'a');
    assert_eq!(eff, LdiscInputEffect::Absorbed);
    assert!(iq.is_empty());
}

// ---------------------------------------------------------------------------
// Output post-processing
// ---------------------------------------------------------------------------

#[test]
fn onlcr_expands_nl_to_crnl() {
    let mut st = LdiscState::new();
    let t = cooked(); // OPOST | ONLCR
    let mut oq: Ring = TtyRing::new();

    process_output(&mut st, &t, &mut oq, b"hi\nbye");
    assert_eq!(drain(&mut oq), b"hi\r\nbye");
}

#[test]
fn opost_off_raw_passthrough() {
    let mut st = LdiscState::new();
    let mut t = cooked();
    t.c_oflag = 0; // clear OPOST
    let mut oq: Ring = TtyRing::new();

    process_output(&mut st, &t, &mut oq, b"hi\nbye");
    assert_eq!(drain(&mut oq), b"hi\nbye");
}

#[test]
fn onocr_suppresses_cr_at_column_zero() {
    let mut st = LdiscState::new();
    let mut t = cooked();
    t.c_oflag = crate::tty::structure::termios::OPOST | crate::tty::structure::termios::ONOCR;
    st.column = 0;
    let mut oq: Ring = TtyRing::new();

    // CR at column 0 should be suppressed.
    process_output(&mut st, &t, &mut oq, b"\r");
    assert_eq!(drain(&mut oq), b"");

    // After writing a character, column > 0, so CR should pass through.
    process_output(&mut st, &t, &mut oq, b"x\r");
    let out = drain(&mut oq);
    assert!(out.contains(&b'\r'), "CR should appear when column > 0");
}

// ---------------------------------------------------------------------------
// on_termios_changed
// ---------------------------------------------------------------------------

#[test]
fn icanon_off_flushes_pending_cooked_buffer() {
    let mut st = LdiscState::new();
    let old = cooked();
    let mut new = cooked();
    new.c_lflag &= !ICANON;

    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    // Accumulate "hello" into the canonical buffer (no newline).
    feed(&mut st, &old, &mut iq, &mut oq, b"hello");
    assert!(iq.is_empty(), "no commit yet");

    // Switching to non-canonical must flush the pending buffer.
    on_termios_changed(&mut st, &old, &new, &mut iq, &mut oq);
    assert_eq!(drain(&mut iq), b"hello");
    assert!(st.cooked_buf.is_empty());
}

#[test]
fn icanon_already_off_no_double_flush() {
    let mut st = LdiscState::new();
    let old = raw();
    let new = raw();
    let mut iq: Ring = TtyRing::new();
    let mut oq: Ring = TtyRing::new();

    on_termios_changed(&mut st, &old, &new, &mut iq, &mut oq);
    assert!(iq.is_empty());
}

// ---------------------------------------------------------------------------
// Phase C execution steps
// ---------------------------------------------------------------------------

#[test]
fn step_ingest_commits_cooked_line_and_step_read_drains_it() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        0,
        "ttyS0",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let mut out = [0u8; 8];

    assert_eq!(
        step_ingest(&tty, b"abc", &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 3,
            writable_fired: true,
            ..Default::default()
        })
    );
    assert!(matches!(
        step_read(&tty, &mut out, &guard),
        StepOutcome::Blocked(_)
    ));

    let outcome = step_ingest(&tty, b"\n", &guard);
    assert!(matches!(
        outcome,
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: true,
            ..
        })
    ));
    assert_eq!(tty.input_readable.peek() & TTY_READABLE, TTY_READABLE);
    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(4));
    assert_eq!(&out[..4], b"abc\n");
}

#[test]
fn step_ingest_empty_veof_makes_next_read_return_zero() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        1,
        "ttyS1",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let mut out = [0u8; 8];

    let outcome = step_ingest(&tty, &[0x04], &guard);
    assert!(matches!(
        outcome,
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: true,
            ..
        })
    ));
    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(0));
}

#[test]
fn step_write_to_pty_peer_ingests_peer_input() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let peer = alloc_tty(
        TtyKind::PtySlave,
        0,
        "pts/0",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    let writer = alloc_tty(
        TtyKind::PtyMaster,
        0,
        "ptmx",
        TtyPayload::new_pty_master(peer.clone()),
    );
    let mut out = [0u8; 8];

    use tx_substrate::step_v3::StepOutcome as V3Out;
    assert_eq!(step_write(&writer, b"hi", &guard), V3Out::Done(2));
    assert!(matches!(
        step_read(&peer, &mut out, &guard),
        StepOutcome::Blocked(_)
    ));
    assert_eq!(step_write(&writer, b"\n", &guard), V3Out::Done(1));
    assert_eq!(step_read(&peer, &mut out, &guard), StepOutcome::Done(3));
    assert_eq!(&out[..3], b"hi\n");
}

#[test]
fn project_devfs_materializes_hardware_tty_and_console_alias() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let tty = match register_hardware("ttyS0", 0, &NOOP_BINDING, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty.clone()),
        StepOutcome::Done(())
    );

    let by_name = match crate::tty::project::devfs_tty_by_name(b"ttyS0", &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("ttyS0 lookup failed: {other:?}"),
    };
    let console = match crate::tty::project::devfs_tty_by_name(b"console", &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("console lookup failed: {other:?}"),
    };
    assert_eq!(by_name, tty);
    assert_eq!(console, tty);

    let rnode = match crate::tty::project::devfs_rnode_by_name(b"console", &guard) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("console rnode failed: {other:?}"),
    };
    assert!(matches!(
        rnode.backing(),
        crate::vfs::RNodeBacking::StructBacked {
            payload: crate::vfs::StructPayload::Tty(r_tty)
        } if *r_tty == tty
    ));
}

#[test]
fn project_open_devfs_tty_by_name_shares_identity_between_console_and_ttys0() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let tty = match register_hardware("ttyS0", 0, &NOOP_BINDING, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty.clone()),
        StepOutcome::Done(())
    );

    let ttys0_file = match crate::tty::project::open_devfs_tty_by_name(b"ttyS0", &guard) {
        StepOutcome::Done(file) => file,
        other => panic!("open ttyS0 failed: {other:?}"),
    };
    let console_file = match crate::tty::project::open_devfs_tty_by_name(b"console", &guard) {
        StepOutcome::Done(file) => file,
        other => panic!("open console failed: {other:?}"),
    };

    assert!(matches!(
        ttys0_file.rnode().backing(),
        crate::vfs::RNodeBacking::StructBacked {
            payload: crate::vfs::StructPayload::Tty(r_tty)
        } if *r_tty == tty
    ));
    assert!(matches!(
        console_file.rnode().backing(),
        crate::vfs::RNodeBacking::StructBacked {
            payload: crate::vfs::StructPayload::Tty(r_tty)
        } if *r_tty == tty
    ));

    assert_eq!(
        ttys0_file.step_write(b"ttyS0\n", &guard),
        StepOutcome::Done(6)
    );
    assert_eq!(
        console_file.step_write(b"console\n", &guard),
        StepOutcome::Done(8)
    );
}

#[test]
fn step_poll_hardware_input_ingests_uart_bytes_into_registered_console_tty() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let ops = Box::leak(Box::new(ScriptedReadOps::new(b"hello\n")));
    let binding = Box::leak(Box::new(CharDeviceBinding {
        devt: DevT::new(4, 65),
        name: "ttyS1",
        ops,
    }));

    let tty = match register_hardware("ttyS1", 1, binding, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty.clone()),
        StepOutcome::Done(())
    );

    let outcome = step_poll_hardware_input(&tty, 16, &guard);
    assert_eq!(
        outcome,
        StepOutcome::Done(crate::tty::execution::HardwarePollOutcome {
            bytes_read: 6,
            ingest: crate::tty::execution::IngestOutcome {
                consumed: 6,
                readable_fired: true,
                writable_fired: true,
                deferred_signal: None,
                signal_dispatch: None,
                flow_control: None,
            },
        })
    );

    let console_file = match crate::tty::project::open_devfs_tty_by_name(b"console", &guard) {
        StepOutcome::Done(file) => file,
        other => panic!("open console failed: {other:?}"),
    };
    let mut out = [0u8; 16];
    assert_eq!(
        console_file.step_read(&mut out, &guard),
        StepOutcome::Done(6)
    );
    assert_eq!(&out[..6], b"hello\n");

    assert_eq!(
        step_poll_hardware_input(&tty, 16, &guard),
        StepOutcome::Done(crate::tty::execution::HardwarePollOutcome::default())
    );
    assert!(ops.recorded_writes().is_empty());
}

#[test]
fn ioctl_binding_and_termios_roundtrip_work() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        2,
        "ttyS2",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    let leader = IoctlCaller::new(11, 22).as_session_leader();
    assert_eq!(
        step_ioctl_tiocsctty(&tty, leader, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    assert_eq!(
        tty.session_pgrp(),
        Some(SessionPgrp::from_raw_ids(11, 22, 22))
    );
    assert_eq!(step_ioctl_tiocgpgrp(&tty, &guard), StepOutcome::Done(22));
    assert_eq!(
        step_ioctl_tiocspgrp(&tty, leader, 33, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    assert_eq!(step_ioctl_tiocgpgrp(&tty, &guard), StepOutcome::Done(33));

    assert_eq!(
        step_ioctl_tiocgwinsz(&tty, &guard),
        StepOutcome::Done(Winsize::default())
    );
    assert_eq!(
        step_ioctl_tiocswinsz(&tty, Winsize::new(24, 80), &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: Some(SignalDispatch {
                target: SignalTarget::ForegroundProcessGroup {
                    pgid: 33,
                    pgrp: None
                },
                signal: JobControlSignal::Winch,
            }),
        })
    );
    assert_eq!(
        step_ioctl_tiocgwinsz(&tty, &guard),
        StepOutcome::Done(Winsize::new(24, 80))
    );

    let mut new_termios = cooked();
    new_termios.c_lflag &= !ICANON;
    assert_eq!(
        step_ioctl_tcsets(&tty, new_termios, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect::default())
    );
    assert_eq!(
        step_ioctl_tcgets(&tty, &guard),
        StepOutcome::Done(new_termios)
    );

    assert_eq!(
        step_ioctl_tiocnotty(&tty, IoctlCaller::new(11, 33), &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    assert_eq!(tty.session_pgrp(), None);
}

#[test]
fn ioctl_rejects_rebind_wrong_session_and_detach_without_binding() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        7,
        "ttyS7",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    let leader = IoctlCaller::new(70, 71).as_session_leader();
    assert_eq!(
        step_ioctl_tiocsctty(&tty, leader, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    assert_eq!(
        step_ioctl_tiocsctty(&tty, IoctlCaller::new(72, 73).as_session_leader(), &guard),
        StepOutcome::Err(Errno::EBUSY)
    );
    assert_eq!(
        step_ioctl_tiocspgrp(&tty, IoctlCaller::new(99, 71), 80, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
    assert_eq!(
        step_ioctl_tiocnotty(&tty, IoctlCaller::new(99, 71), &guard),
        StepOutcome::Err(Errno::EINVAL)
    );

    let unbound = alloc_tty(
        TtyKind::SerialHardware,
        8,
        "ttyS8",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    assert_eq!(
        step_ioctl_tiocnotty(&unbound, IoctlCaller::new(1, 1), &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
    assert_eq!(
        step_ioctl_tiocgpgrp(&unbound, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
}

#[test]
fn step_ingest_reports_foreground_signal_dispatch_when_bound() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::PtySlave,
        3,
        "pts/3",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(7, 9, 9));

    assert_eq!(
        step_ingest(&tty, &[0x03], &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: false,
            writable_fired: false,
            deferred_signal: Some(crate::tty::execution::DeferredSignalEvent {
                signal: SignalKind::Int,
            }),
            signal_dispatch: Some(SignalDispatch {
                target: SignalTarget::ForegroundProcessGroup {
                    pgid: 9,
                    pgrp: None
                },
                signal: JobControlSignal::Int,
            }),
            flow_control: None,
        })
    );
}

#[test]
fn step_ingest_signal_dispatch_is_none_without_controlling_binding() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::PtySlave,
        9,
        "pts/9",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    assert_eq!(
        step_ingest(&tty, &[0x03], &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: false,
            writable_fired: false,
            deferred_signal: Some(crate::tty::execution::DeferredSignalEvent {
                signal: SignalKind::Int,
            }),
            signal_dispatch: None,
            flow_control: None,
        })
    );
}

#[test]
fn step_ingest_reports_sigquit_and_sigtstp_dispatch_when_bound() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::PtySlave,
        12,
        "pts/12",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(12, 120, 121));

    assert_eq!(
        step_ingest(&tty, &[0x1c], &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: false,
            writable_fired: false,
            deferred_signal: Some(crate::tty::execution::DeferredSignalEvent {
                signal: SignalKind::Quit,
            }),
            signal_dispatch: Some(SignalDispatch {
                target: SignalTarget::ForegroundProcessGroup {
                    pgid: 121,
                    pgrp: None
                },
                signal: JobControlSignal::Quit,
            }),
            flow_control: None,
        })
    );

    assert_eq!(
        step_ingest(&tty, &[0x1a], &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 1,
            readable_fired: false,
            writable_fired: false,
            deferred_signal: Some(crate::tty::execution::DeferredSignalEvent {
                signal: SignalKind::Tstp,
            }),
            signal_dispatch: Some(SignalDispatch {
                target: SignalTarget::ForegroundProcessGroup {
                    pgid: 121,
                    pgrp: None
                },
                signal: JobControlSignal::Tstp,
            }),
            flow_control: None,
        })
    );
}

#[test]
fn background_write_with_tostop_returns_eio() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        4,
        "ttyS4",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(1, 10, 10));
    let mut termios = match step_ioctl_tcgets(&tty, &guard) {
        StepOutcome::Done(termios) => termios,
        other => panic!("tcgets failed: {other:?}"),
    };
    termios.c_lflag |= TOSTOP;
    assert_eq!(
        step_ioctl_tcsets(&tty, termios, &guard),
        StepOutcome::Done(Default::default())
    );

    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Out};
    let bg = IoctlCaller::new(1, 11).background();
    assert_eq!(
        step_write_for_caller(&tty, b"x", bg, &guard),
        V3Out::Err(V3Errno::EIO)
    );

    let ignored = IoctlCaller::new(1, 11).background().ignore_sigttou();
    assert_eq!(
        step_write_for_caller(&tty, b"x", ignored, &guard),
        V3Out::Err(V3Errno::EIO)
    );
}

#[test]
fn background_read_returns_eio_in_staging_path() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        6,
        "ttyS6",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(2, 20, 20));

    let mut out = [0u8; 8];
    let bg = IoctlCaller::new(2, 21).background();
    assert_eq!(
        step_read_for_caller(&tty, &mut out, bg, &guard),
        StepOutcome::Err(Errno::EIO)
    );

    // v1 staging simplification: ignored SIGTTIN still reports EIO here until
    // caller-aware job-control is wired into the real fd read path.
    let ignored = IoctlCaller::new(2, 21).background().ignore_sigttin();
    assert_eq!(
        step_read_for_caller(&tty, &mut out, ignored, &guard),
        StepOutcome::Err(Errno::EIO)
    );
}

#[test]
fn tcsets_flushes_pending_cooked_buffer_into_read_queue() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        11,
        "ttyS11",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    assert_eq!(
        step_ingest(&tty, b"flush-me", &guard),
        StepOutcome::Done(crate::tty::execution::IngestOutcome {
            consumed: 8,
            writable_fired: true,
            ..Default::default()
        })
    );

    let mut out = [0u8; 16];
    assert!(matches!(
        step_read(&tty, &mut out, &guard),
        StepOutcome::Blocked(_)
    ));

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
    assert_eq!(tty.input_readable.peek() & TTY_READABLE, TTY_READABLE);
    assert_eq!(step_read(&tty, &mut out, &guard), StepOutcome::Done(8));
    assert_eq!(&out[..8], b"flush-me");
}

#[test]
fn step_hangup_and_master_close_drop_payload_and_emit_signals() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let tty = alloc_tty(
        TtyKind::SerialHardware,
        5,
        "ttyS5",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(5, 51, 50));
    assert_eq!(
        step_hangup(&tty, &guard),
        StepOutcome::Done(crate::tty::execution::HangupOutcome {
            had_payload: true,
            hangup_fired: true,
            session_ctl_fired: true,
            hup_signal: Some(SignalDispatch {
                target: SignalTarget::SessionLeaderProcessGroup {
                    pgid: 51,
                    pgrp: None
                },
                signal: JobControlSignal::Hup,
            }),
            cont_signal: Some(SignalDispatch {
                target: SignalTarget::ForegroundProcessGroup {
                    pgid: 50,
                    pgrp: None
                },
                signal: JobControlSignal::Cont,
            }),
        })
    );
    assert!(!tty.is_live());
    assert_eq!(tty.session_pgrp(), None);

    let pty = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("open_ptmx failed: {other:?}"),
    };
    pty.slave
        .bind_session_pgrp(SessionPgrp::from_raw_ids(6, 61, 60));
    let close = step_master_close_last(&pty.master, &guard);
    assert!(matches!(
        close,
        StepOutcome::Done((
            crate::tty::execution::HangupOutcome {
                had_payload: true,
                hangup_fired: true,
                session_ctl_fired: true,
                ..
            },
            crate::tty::execution::IoctlSideEffect { .. }
        ))
    ));
    assert!(!pty.slave.is_live());
}

#[test]
fn slave_close_does_not_hangup_master() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let pty = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("open_ptmx failed: {other:?}"),
    };

    let _ = pty.slave.take_payload();
    assert!(!pty.slave.is_live(), "slave payload should be gone");
    assert!(pty.master.is_live(), "slave close must not hang up master");
    use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Out};
    assert_eq!(
        step_write(&pty.master, b"x", &guard),
        V3Out::Err(V3Errno::EIO)
    );
}

#[test]
fn hangup_makes_followup_io_return_eio_and_second_hangup_is_eio() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        10,
        "ttyS10",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    assert!(matches!(step_hangup(&tty, &guard), StepOutcome::Done(_)));

    let mut out = [0u8; 4];
    assert_eq!(
        step_read(&tty, &mut out, &guard),
        StepOutcome::Err(Errno::EIO)
    );
    {
        use tx_substrate::step_v3::{Errno as V3Errno, StepOutcome as V3Out};
        assert_eq!(step_write(&tty, b"x", &guard), V3Out::Err(V3Errno::EIO));
    }
    assert_eq!(step_hangup(&tty, &guard), StepOutcome::Err(Errno::EIO));
}

#[test]
fn hangup_preserves_identity_but_removes_live_payload() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        13,
        "ttyS13",
        TtyPayload::new_hardware(&NOOP_BINDING),
    );

    assert!(tty.live_payload().is_some());
    assert!(matches!(step_hangup(&tty, &guard), StepOutcome::Done(_)));
    assert!(tty.live_payload().is_none(), "payload should be withdrawn");
    assert_eq!(
        tty.name.as_bytes(),
        b"ttyS13",
        "identity should remain observable"
    );
}

#[test]
fn pty_registry_unregister_removes_entry_and_next_open_reuses_slot_space() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let first = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("first open_ptmx failed: {other:?}"),
    };
    assert!(crate::tty::structure::registry::contains_pty_slave(
        first.index
    ));
    let removed = crate::tty::structure::registry::unregister_pty_slave(first.index);
    assert!(removed.is_some(), "pty slave should unregister");
    assert!(!crate::tty::structure::registry::contains_pty_slave(
        first.index
    ));
    assert!(matches!(
        crate::tty::project::devpts_slave_by_index(first.index, &guard),
        StepOutcome::Err(Errno::ENOENT)
    ));

    let second = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("second open_ptmx failed: {other:?}"),
    };
    assert!(
        second.index == first.index.wrapping_add(1) || second.index == first.index,
        "pty allocator should continue providing available slots"
    );
}

#[test]
fn pty_index_allocation_is_monotonic_while_entries_remain_live() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let first = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("first open_ptmx failed: {other:?}"),
    };
    let second = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("second open_ptmx failed: {other:?}"),
    };
    let third = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("third open_ptmx failed: {other:?}"),
    };

    assert_eq!(first.index, 0);
    assert_eq!(second.index, 1);
    assert_eq!(third.index, 2);
}

#[test]
fn project_open_ptmx_registers_devpts_slave_and_files_are_usable() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();

    let pty = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("open_ptmx failed: {other:?}"),
    };
    let slave = match crate::tty::project::devpts_slave_by_index(pty.index, &guard) {
        StepOutcome::Done(slave) => slave,
        other => panic!("devpts slave lookup failed: {other:?}"),
    };
    assert_eq!(slave, pty.slave);

    let slave_rnode = match crate::tty::project::devpts_rnode_by_index(pty.index, &guard) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("devpts rnode failed: {other:?}"),
    };
    assert!(matches!(
        slave_rnode.backing(),
        crate::vfs::RNodeBacking::StructBacked {
            payload: crate::vfs::StructPayload::Tty(r_tty)
        } if *r_tty == pty.slave
    ));

    assert_eq!(
        pty.master_file.step_write(b"from master\n", &guard),
        StepOutcome::Done(12)
    );
    let mut out = [0u8; 16];
    assert_eq!(
        pty.slave_file.step_read(&mut out, &guard),
        StepOutcome::Done(12)
    );
    assert_eq!(&out[..12], b"from master\n");
}

#[test]
fn devpts_fs_lookup_meta_and_readdir_follow_registry() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = crate::tty::project::DevptsInstance;

    let p0 = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("first open_ptmx failed: {other:?}"),
    };
    let p1 = match crate::tty::project::open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("second open_ptmx failed: {other:?}"),
    };

    {
        use tx_substrate::step_v3::{Errno, StepOutcome};
        assert_eq!(
            devpts.lookup(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, b"ptmx", &guard),
            StepOutcome::Done(crate::tty::project::DEVPTS_PTMX_OBJECT_ID)
        );
        let slave0_rnode = match crate::tty::project::devpts_rnode_by_index(p0.index, &guard) {
            super::super::super::execution::StepOutcome::Done(rnode) => rnode,
            other => panic!("devpts slave 0 rnode failed: {other:?}"),
        };
        let slave1_rnode = match crate::tty::project::devpts_rnode_by_index(p1.index, &guard) {
            super::super::super::execution::StepOutcome::Done(rnode) => rnode,
            other => panic!("devpts slave 1 rnode failed: {other:?}"),
        };
        assert_eq!(
            devpts.lookup(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, b"0", &guard),
            StepOutcome::Done(slave0_rnode.fs_object_id())
        );
        assert_eq!(
            devpts.lookup(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, b"1", &guard),
            StepOutcome::Done(slave1_rnode.fs_object_id())
        );
        assert_eq!(
            devpts.lookup(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, b"99", &guard),
            StepOutcome::Err(Errno::ENOENT)
        );

        assert_eq!(
            devpts.load_inode_meta(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, &guard),
            StepOutcome::Done(crate::vfs::InodeMeta::new(InodeKind::Directory, 0o040755))
        );
        assert_eq!(
            devpts.load_inode_meta(crate::tty::project::DEVPTS_PTMX_OBJECT_ID, &guard),
            StepOutcome::Done(crate::vfs::InodeMeta::new(InodeKind::CharDevice, 0o020666))
        );

        let mut names = Vec::new();
        let mut cursor = DirCursor::START;
        loop {
            match devpts.readdir(crate::tty::project::DEVPTS_ROOT_OBJECT_ID, cursor, &guard) {
                StepOutcome::Done(Some((entry, next))) => {
                    names.push(entry.name.as_bytes().to_vec());
                    cursor = next;
                }
                StepOutcome::Done(None) => break,
                other => panic!("readdir failed: {other:?}"),
            }
        }

        assert_eq!(names.len(), 3);
        assert_eq!(names[0], b"ptmx".to_vec());
        assert_eq!(names[1], b"0".to_vec());
        assert_eq!(names[2], b"1".to_vec());
    }
}

#[test]
fn devpts_fs_is_read_only_and_root_only() {
    use tx_substrate::step_v3::Errno as V3Errno;
    use tx_substrate::step_v3::StepOutcome as V3Outcome;
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = crate::tty::project::DevptsInstance;
    let cred = Credential::default();

    assert_eq!(
        devpts.lookup(crate::vfs::FsObjectId::new(9), b"ptmx", &guard),
        V3Outcome::Err(V3Errno::ENOENT)
    );
    assert_eq!(
        devpts.readdir(
            crate::tty::project::DEVPTS_PTMX_OBJECT_ID,
            DirCursor::START,
            &guard
        ),
        V3Outcome::Err(V3Errno::ENOTDIR)
    );
    assert_eq!(
        devpts.serialize_inode_meta(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            &crate::vfs::InodeMeta::new(InodeKind::Directory, 0o040755),
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.create_inode(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"2",
            0o020600,
            &cred,
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.unlink(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"0",
            crate::vfs::FsObjectId::new(0),
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.rename(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"0",
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"1",
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.link(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"0",
            crate::vfs::FsObjectId::new(0),
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.mkdir(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"dir",
            0o040755,
            &cred,
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.rmdir(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"dir",
            crate::vfs::FsObjectId::new(0),
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
    assert_eq!(
        devpts.symlink(
            crate::tty::project::DEVPTS_ROOT_OBJECT_ID,
            b"link",
            b"target",
            &cred,
            &guard,
        ),
        V3Outcome::Err(V3Errno::EROFS)
    );
}
