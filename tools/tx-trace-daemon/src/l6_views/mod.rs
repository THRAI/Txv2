//! L6 host projections and derived views.
//!
//! JSON and Perfetto outputs are views over L5 canonical events. They must not
//! become independent sources of txtrace schema truth.

use crate::l5_canonical::TraceEventStream;

pub mod emit_json;
pub mod perfetto;

/// L6 projection input. Views consume canonical streams, not L4 readers.
#[derive(Clone, Copy)]
pub struct ProjectionInput<'a> {
    pub stream: &'a TraceEventStream,
}

impl<'a> ProjectionInput<'a> {
    pub fn new(stream: &'a TraceEventStream) -> Self {
        Self { stream }
    }
}

/// View/exporter contract for host projections.
#[allow(dead_code)]
pub trait TraceTranscoder {
    type Output;

    fn transcode(&mut self, input: ProjectionInput<'_>) -> std::io::Result<Self::Output>;
}
