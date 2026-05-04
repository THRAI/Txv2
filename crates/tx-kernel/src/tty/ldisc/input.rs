//! N_TTY input-side pipeline.
//!
//! `process_input_byte` is the single entry point for the input pipeline.
//! It processes one byte from the transport and returns a discriminated
//! `LdiscInputEffect` describing the byte's fate. Signal dispatch, wire
//! fires, and queue wake-ups are the caller's responsibility — the ldisc
//! has no knowledge of processes, sessions, or reactor primitives.
//!
//! Pipeline order (per TTY.md §3.2 and the design plan §A.5):
//!
//! 1. VLNEXT escape check (takes priority over everything)
//! 2. Input translation (`ICRNL` / `INLCR` / `IGNCR`)
//! 3. Signal generation (`ISIG` + VINTR / VQUIT / VSUSP)
//! 4. Flow control (`IXON` + VSTOP / VSTART / IXANY)
//! 5. VLNEXT activation (`ICANON` + `IEXTEN`)
//! 6. Canonical or non-canonical processing

use crate::tty::ldisc::effect::{FlowCtl, LdiscInputEffect, SignalKind};
use crate::tty::ldisc::state::LdiscState;
use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::{
    Termios, ECHO, ECHOE, ECHOK, ECHONL, ICANON, ICRNL, IEXTEN, IGNCR, INLCR, ISIG, IXANY, IXON,
    POSIX_VDISABLE, VEOF, VEOL, VERASE, VINTR, VKILL, VLNEXT, VQUIT, VSTART, VSTOP, VSUSP,
};

/// Process one input byte through the N_TTY line discipline.
///
/// Mutates `state`, may push echo bytes to `output_queue`, may push committed
/// bytes to `input_queue`. Returns the effect describing what happened.
pub fn process_input_byte<const IC: usize, const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    input_queue: &mut TtyRing<IC>,
    output_queue: &mut TtyRing<OC>,
    byte: u8,
) -> LdiscInputEffect {
    // 1. VLNEXT escape: the previous byte was ^V, so this byte is literal.
    if state.lnext {
        state.lnext = false;
        return handle_literal(state, termios, input_queue, output_queue, byte);
    }

    // 2. Input translation: ICRNL / INLCR / IGNCR.
    let byte = match translate_input(termios, byte) {
        Some(b) => b,
        None => return LdiscInputEffect::Absorbed, // IGNCR: silently discard CR
    };

    // 3. Signal generation: ISIG + VINTR / VQUIT / VSUSP.
    if termios.c_lflag & ISIG != 0 {
        if let Some(sig) = signal_for_byte(termios, byte) {
            return LdiscInputEffect::SignalFgPgrp(sig);
        }
    }

    // 4. Flow control: IXON + VSTOP / VSTART / IXANY.
    if termios.c_iflag & IXON != 0 {
        let vstop = termios.c_cc[VSTOP];
        let vstart = termios.c_cc[VSTART];

        if byte == vstop && vstop != POSIX_VDISABLE {
            state.flow_stopped = true;
            return LdiscInputEffect::FlowControl(FlowCtl::Stop);
        }
        if byte == vstart && vstart != POSIX_VDISABLE {
            state.flow_stopped = false;
            return LdiscInputEffect::FlowControl(FlowCtl::Start);
        }
        if state.flow_stopped {
            if termios.c_iflag & IXANY != 0 {
                // Any character resumes output; continue processing this byte.
                state.flow_stopped = false;
            } else {
                return LdiscInputEffect::Absorbed;
            }
        }
    }

    // 5. VLNEXT activation: ICANON + IEXTEN.
    if termios.c_lflag & ICANON != 0 && termios.c_lflag & IEXTEN != 0 {
        let vlnext = termios.c_cc[VLNEXT];
        if byte == vlnext && vlnext != POSIX_VDISABLE {
            state.lnext = true;
            return LdiscInputEffect::Absorbed;
        }
    }

    // 6. Route to canonical or non-canonical processing.
    if termios.c_lflag & ICANON != 0 {
        process_canonical(state, termios, input_queue, output_queue, byte)
    } else {
        process_raw(termios, input_queue, output_queue, byte)
    }
}

// ---------------------------------------------------------------------------
// Input translation
// ---------------------------------------------------------------------------

fn translate_input(termios: &Termios, byte: u8) -> Option<u8> {
    match byte {
        b'\r' if termios.c_iflag & IGNCR != 0 => None,
        b'\r' if termios.c_iflag & ICRNL != 0 => Some(b'\n'),
        b'\n' if termios.c_iflag & INLCR != 0 => Some(b'\r'),
        other => Some(other),
    }
}

// ---------------------------------------------------------------------------
// Signal detection
// ---------------------------------------------------------------------------

fn signal_for_byte(termios: &Termios, byte: u8) -> Option<SignalKind> {
    let cc = &termios.c_cc;
    if byte == cc[VINTR] && cc[VINTR] != POSIX_VDISABLE {
        return Some(SignalKind::Int);
    }
    if byte == cc[VQUIT] && cc[VQUIT] != POSIX_VDISABLE {
        return Some(SignalKind::Quit);
    }
    if byte == cc[VSUSP] && cc[VSUSP] != POSIX_VDISABLE {
        return Some(SignalKind::Tstp);
    }
    None
}

// ---------------------------------------------------------------------------
// VLNEXT literal pass-through
// ---------------------------------------------------------------------------

/// Handle the byte following a VLNEXT escape: treat it as literal input,
/// bypassing all special-character interpretation.
fn handle_literal<const IC: usize, const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    input_queue: &mut TtyRing<IC>,
    output_queue: &mut TtyRing<OC>,
    byte: u8,
) -> LdiscInputEffect {
    if termios.c_lflag & ICANON != 0 {
        echo_byte(termios, output_queue, byte);
        let _ = state.cooked_buf.push(byte);
        LdiscInputEffect::Absorbed
    } else {
        echo_byte(termios, output_queue, byte);
        let _ = input_queue.push(byte);
        LdiscInputEffect::QueuedForRead
    }
}

// ---------------------------------------------------------------------------
// Canonical (ICANON) processing
// ---------------------------------------------------------------------------

fn process_canonical<const IC: usize, const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    input_queue: &mut TtyRing<IC>,
    output_queue: &mut TtyRing<OC>,
    byte: u8,
) -> LdiscInputEffect {
    let cc = &termios.c_cc;

    // VERASE: delete the most recently typed byte from the canonical buffer.
    if byte == cc[VERASE] && cc[VERASE] != POSIX_VDISABLE {
        if state.cooked_buf.pop_back().is_some() {
            echo_erase(termios, output_queue);
        }
        return LdiscInputEffect::Absorbed;
    }

    // VKILL: erase the entire canonical buffer.
    if byte == cc[VKILL] && cc[VKILL] != POSIX_VDISABLE {
        state.cooked_buf.clear();
        echo_kill(termios, output_queue);
        return LdiscInputEffect::Absorbed;
    }

    // VEOF: flush cooked_buf to input_queue without appending the EOF byte.
    //
    // An empty flush (VEOF on an empty line) causes read() to return 0 bytes,
    // which the POSIX caller interprets as EOF. Phase C's step_read handles
    // the zero-length case; here we always return LineCommitted.
    if byte == cc[VEOF] && cc[VEOF] != POSIX_VDISABLE {
        state.cooked_buf.drain_into(input_queue);
        return LdiscInputEffect::LineCommitted;
    }

    // VEOL / newline: append to buffer and flush the completed line.
    let is_eol = byte == cc[VEOL] && cc[VEOL] != POSIX_VDISABLE;
    if byte == b'\n' || is_eol {
        let _ = state.cooked_buf.push(byte);
        echo_newline(termios, output_queue);
        state.cooked_buf.drain_into(input_queue);
        return LdiscInputEffect::LineCommitted;
    }

    // MAX_CANON overflow: flush what we have, then push the overflow byte
    // directly to input_queue so it is not silently discarded.
    if state.cooked_buf.is_full() {
        state.cooked_buf.drain_into(input_queue);
        let _ = input_queue.push(byte);
        return LdiscInputEffect::LineCommitted;
    }

    // Normal canonical character: accumulate and optionally echo.
    let _ = state.cooked_buf.push(byte);
    echo_byte(termios, output_queue, byte);
    LdiscInputEffect::Absorbed
}

// ---------------------------------------------------------------------------
// Non-canonical (raw) processing
// ---------------------------------------------------------------------------

fn process_raw<const IC: usize, const OC: usize>(
    termios: &Termios,
    input_queue: &mut TtyRing<IC>,
    output_queue: &mut TtyRing<OC>,
    byte: u8,
) -> LdiscInputEffect {
    echo_byte(termios, output_queue, byte);
    let _ = input_queue.push(byte);
    LdiscInputEffect::QueuedForRead
}

// ---------------------------------------------------------------------------
// Echo helpers
// ---------------------------------------------------------------------------

fn echo_byte<const OC: usize>(termios: &Termios, output_queue: &mut TtyRing<OC>, byte: u8) {
    if termios.c_lflag & ECHO != 0 {
        let _ = output_queue.push(byte);
    }
}

/// Echo a newline: fires when `\n` or VEOL commits a line.
///
/// Echoes even if `ECHO` is off, provided `ECHONL` is set.
fn echo_newline<const OC: usize>(termios: &Termios, output_queue: &mut TtyRing<OC>) {
    if termios.c_lflag & (ECHO | ECHONL) != 0 {
        let _ = output_queue.push(b'\n');
    }
}

/// Echo a VERASE: `BS SP BS` when `ECHOE` is set, otherwise the erase char.
fn echo_erase<const OC: usize>(termios: &Termios, output_queue: &mut TtyRing<OC>) {
    if termios.c_lflag & ECHO == 0 {
        return;
    }
    if termios.c_lflag & ECHOE != 0 {
        let _ = output_queue.push(b'\x08'); // BS
        let _ = output_queue.push(b' ');
        let _ = output_queue.push(b'\x08'); // BS
    } else {
        let _ = output_queue.push(termios.c_cc[VERASE]);
    }
}

/// Echo a VKILL: `\n` when `ECHOK` is set, otherwise the kill char.
fn echo_kill<const OC: usize>(termios: &Termios, output_queue: &mut TtyRing<OC>) {
    if termios.c_lflag & ECHO == 0 {
        return;
    }
    if termios.c_lflag & ECHOK != 0 {
        let _ = output_queue.push(b'\n');
    } else {
        let _ = output_queue.push(termios.c_cc[VKILL]);
    }
}
