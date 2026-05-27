//! N_TTY line discipline.
//!
//! Hardcoded single line discipline per the LDSC-1 commitment in TTY.md §3:
//! no trait, no vtable, no registry. Behavioral differences between use cases
//! (cooked shell vs. raw pty master) are expressed through termios flags, not
//! discipline variants.

pub mod effect;
pub mod input;
pub mod output;
pub mod state;
pub mod termios_change;

pub use effect::{FlowCtl, LdiscInputEffect, SignalKind};
pub use input::process_input_byte;
pub use output::process_output;
pub use state::LdiscState;
pub use termios_change::on_termios_changed;
