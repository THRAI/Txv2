//! N_TTY output-side pipeline.
//!
//! `process_output` applies `OPOST` transformations to bytes written by the
//! user and places the result in `output_queue`. No signal generation, no
//! canonical buffering — output is strictly unidirectional.
//!
//! Pipeline (per TTY.md §3.3 and design plan §A.6):
//!
//! - `OPOST + ONLCR`: `\n` → `\r\n`
//! - `OPOST + OCRNL`: `\r` → `\n`
//! - `OPOST + ONOCR`: suppress `\r` when `column == 0`
//! - `OPOST + ONLRET`: `\n` resets `column`
//! - `state.column` is updated for every output byte

use crate::tty::ldisc::state::LdiscState;
use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::{Termios, OCRNL, ONLCR, ONLRET, ONOCR, OPOST};

/// Apply `OPOST` transformations and push the result to `output_queue`.
///
/// Returns the number of *input* bytes consumed (not output bytes produced,
/// which may differ because `\n` expands to `\r\n` under `ONLCR`). Processing
/// stops early if `output_queue` has insufficient space.
pub fn process_output<const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    output_queue: &mut TtyRing<OC>,
    bytes: &[u8],
) -> usize {
    if termios.c_oflag & OPOST == 0 {
        // Raw passthrough: no column tracking in raw mode.
        return output_queue.extend_from_slice(bytes);
    }

    let mut written = 0;
    for &byte in bytes {
        let ok = match byte {
            b'\n' => push_nl(state, termios, output_queue),
            b'\r' => push_cr(state, termios, output_queue),
            other => {
                if output_queue.push(other).is_err() {
                    false
                } else {
                    state.column = state.column.saturating_add(1);
                    true
                }
            }
        };
        if ok {
            written += 1;
        } else {
            break;
        }
    }
    written
}

// ---------------------------------------------------------------------------
// Per-character helpers
// ---------------------------------------------------------------------------

fn push_nl<const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    output_queue: &mut TtyRing<OC>,
) -> bool {
    if termios.c_oflag & ONLCR != 0 {
        // \n → \r\n: need two bytes of space.
        if output_queue.space() < 2 {
            return false;
        }
        let _ = output_queue.push(b'\r');
        let _ = output_queue.push(b'\n');
    } else {
        if output_queue.push(b'\n').is_err() {
            return false;
        }
    }
    if termios.c_oflag & ONLRET != 0 {
        state.column = 0;
    }
    true
}

fn push_cr<const OC: usize>(
    state: &mut LdiscState,
    termios: &Termios,
    output_queue: &mut TtyRing<OC>,
) -> bool {
    if termios.c_oflag & OCRNL != 0 {
        // \r → \n (OCRNL).
        if output_queue.push(b'\n').is_err() {
            return false;
        }
        if termios.c_oflag & ONLRET != 0 {
            state.column = 0;
        }
    } else if termios.c_oflag & ONOCR != 0 && state.column == 0 {
        // Suppress \r at column 0 (ONOCR). Byte is silently dropped but
        // counted as processed so the caller advances its slice pointer.
    } else {
        if output_queue.push(b'\r').is_err() {
            return false;
        }
        state.column = 0;
    }
    true
}
