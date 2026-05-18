//! JSON serialization of decoded events to stdout (newline-delimited JSON).
//!
//! For OBS-5 the only output mode is newline-delimited JSON to stdout.
//! Each call to [`emit`] writes one JSON object followed by `\n`.

use std::io::{self, Write};

use crate::decode::DecodedEvent;

/// Emit one [`DecodedEvent`] as a newline-delimited JSON record to `stdout`.
///
/// Errors writing to stdout are propagated (e.g. broken pipe is fatal for the
/// daemon in file-replay mode — there is nothing useful to do without output).
pub fn emit(event: &DecodedEvent) -> io::Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, event)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    lock.write_all(b"\n")
}
