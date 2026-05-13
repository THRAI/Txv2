//! Perfetto protobuf emission — OBS-6.
//!
//! Sub-modules:
//! - `proto`    — hand-derived prost message types for the minimal Perfetto schema
//! - `track`    — TrackRegistry (hart / task UUID allocation)
//! - `span`     — SpanTable (begin/end reconstruction)
//! - `flow`     — flow_id computation (SipHash-1-3)
//! - `interned` — interned event-name table
//! - `writer`   — PftraceWriter (packet accumulator → file)

pub mod flow;
pub mod interned;
pub mod proto;
pub mod span;
pub mod track;
pub mod writer;
