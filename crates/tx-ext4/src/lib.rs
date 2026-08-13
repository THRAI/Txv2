//! tx-ext4 adapters.
#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "host-async"))]
extern crate std;

use sync::SpinMutex;

pub mod adapter;

mod mutation_lifecycle;

#[cfg(feature = "host-async")]
pub mod host_async;
pub mod journal;
pub mod mount;
pub mod namespace;
pub mod pager;
pub mod planner;
mod read_backend;
pub mod settlement;
mod sync;

type DiagnosticSink = fn(&str);

static DIAGNOSTIC_SINK: SpinMutex<Option<DiagnosticSink>> = SpinMutex::new(None);

/// Install the board-console sink used by kernel-side ext4 diagnostics.
pub fn install_diagnostic_sink(sink: DiagnosticSink) {
    *DIAGNOSTIC_SINK.lock() = Some(sink);
}

#[cfg(test)]
mod tests_v3;
