//! L2 producer support for bounded per-hart emission.
//!
//! `HartEmitter` and the producer runtime live here. The crate root only
//! re-exports the stable public API during the compatibility stage.

mod dump;
mod hart_local;
mod runtime;

pub use dump::*;
pub(crate) use hart_local::{HartLocalArray, HartLocalOptionArray};
#[cfg(any(test, feature = "testing"))]
pub(crate) use runtime::testing_reset;
pub use runtime::*;
