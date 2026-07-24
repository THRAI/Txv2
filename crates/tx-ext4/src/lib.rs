//! tx-ext4 adapters.
#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "host-async"))]
extern crate std;

use core::fmt::{self, Write};
use sync::SpinMutex;

pub mod adapter;

#[cfg(feature = "host-async")]
pub mod host_async;
pub mod mount;
pub mod namespace;
pub mod pager;
mod read_backend;
mod sync;

type DiagnosticSink = fn(&str);

static DIAGNOSTIC_SINK: SpinMutex<Option<DiagnosticSink>> = SpinMutex::new(None);

/// Install the kernel's board-console sink for concise ext4 writeback errors.
/// Host tests leave it unset; production boot installs one after the console
/// is available and before mounting the root filesystem.
pub fn install_diagnostic_sink(sink: DiagnosticSink) {
    *DIAGNOSTIC_SINK.lock() = Some(sink);
}

pub(crate) fn report_writeback_error(
    inode: u32,
    logical_block: u64,
    error: tx_ext4_format::Ext4FormatError,
) {
    let sink = *DIAGNOSTIC_SINK.lock();
    let Some(sink) = sink else {
        return;
    };
    let mut line = DiagnosticLine::new();
    let _ = write!(
        line,
        "txkernel:ext4:writeback:error:inode={inode}:logical={logical_block}:error={error:?}\n"
    );
    sink(line.as_str());
}

pub(crate) fn report_filesystem_stats(stats: tx_ext4_format::pager::FilesystemStatsLite) {
    let sink = *DIAGNOSTIC_SINK.lock();
    let Some(sink) = sink else {
        return;
    };
    let mut line = DiagnosticLine::new();
    let _ = write!(
        line,
        "txkernel:linkdiag:ext4-statfs:block_size={}:blocks={}:free={}:avail={}:inodes={}:ifree={}\n",
        stats.block_size,
        stats.total_blocks,
        stats.free_blocks,
        stats.available_blocks,
        stats.total_inodes,
        stats.free_inodes,
    );
    sink(line.as_str());
}

pub(crate) fn report_writeback_stats(
    inode: u32,
    logical_block: u64,
    error: tx_ext4_format::Ext4FormatError,
    stats: Option<tx_ext4_format::pager::FilesystemStatsLite>,
) {
    report_writeback_error(inode, logical_block, error);
    let Some(stats) = stats else {
        return;
    };
    let sink = *DIAGNOSTIC_SINK.lock();
    let Some(sink) = sink else {
        return;
    };
    let mut line = DiagnosticLine::new();
    let _ = write!(
        line,
        "txkernel:linkdiag:ext4-writeback-capacity:inode={inode}:logical={logical_block}:blocks={}:free={}:avail={}:ifree={}\n",
        stats.total_blocks,
        stats.free_blocks,
        stats.available_blocks,
        stats.free_inodes,
    );
    sink(line.as_str());
}

struct DiagnosticLine {
    bytes: [u8; 256],
    len: usize,
}

impl DiagnosticLine {
    const fn new() -> Self {
        Self {
            bytes: [0; 256],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // `fmt::Write` only accepts UTF-8 input and copies it verbatim.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

impl fmt::Write for DiagnosticLine {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let remaining = self.bytes.len().saturating_sub(self.len);
        if text.len() > remaining {
            return Err(fmt::Error);
        }
        self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests_v3;
