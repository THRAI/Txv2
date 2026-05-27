//! Error types for zone allocation, publication, and observation.

use crate::epoch::EpochError;
use crate::page_allocator::AllocError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneError {
    /// Zone runtime or registry has already been initialized.
    AlreadyInitialized,
    /// Zone runtime was used before BSP initialization.
    NotInitialized,
    /// Zone runtime has been frozen for shutdown/panic diagnostics.
    FrozenForShutdown,
    /// The target zone has not been registered during boot-time zone setup.
    NotRegistered,
    /// No slab/frame/slot resource was available.
    AllocationFailed,
    /// A slot or runtime state violated the expected lifecycle.
    InvalidState,
    /// A `SlotKey` could not be resolved to a live slot of the requested type.
    SlotNotFound,
    /// Retain count would overflow.
    RetainOverflow,
    /// EBR refused or failed a retirement operation.
    Epoch(EpochError),
}

impl From<EpochError> for ZoneError {
    fn from(value: EpochError) -> Self {
        Self::Epoch(value)
    }
}

impl From<AllocError> for ZoneError {
    fn from(value: AllocError) -> Self {
        match value {
            AllocError::Exhausted => Self::AllocationFailed,
            AllocError::InvalidRequest => Self::InvalidState,
            AllocError::NotInitialized => Self::NotInitialized,
            _ => Self::AllocationFailed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Returned when a weak observation cannot be upgraded to retained evidence.
pub struct Dead;
