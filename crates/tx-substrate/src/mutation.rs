//! Mutation helpers layered over index reservation/commit states.

use crate::index::{Index, IndexError};

/// Mutation helper failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationError {
    /// The key already has a committed or reserved entry.
    AlreadyPresent,
    /// No committed entry exists for the key.
    Missing,
    /// The bounded index has no free slot.
    Full,
    /// The key is currently reserved by another operation.
    Busy,
}

/// Install `value` only if `key` is absent.
pub fn install_if_absent<K: Eq, V, const N: usize>(
    index: &Index<K, V, N>,
    key: K,
    value: V,
) -> Result<(), MutationError> {
    match index.reserve(key) {
        Ok(reservation) => {
            reservation.commit(value);
            Ok(())
        }
        Err(IndexError::Duplicate) => Err(MutationError::AlreadyPresent),
        Err(IndexError::Full) => Err(MutationError::Full),
        Err(IndexError::Missing) => Err(MutationError::Missing),
        Err(IndexError::Busy) => Err(MutationError::Busy),
    }
}

/// Withdraw and return a committed value.
pub fn withdraw<K: Eq, V, const N: usize>(
    index: &Index<K, V, N>,
    key: &K,
) -> Result<V, MutationError> {
    Ok(index
        .reserve_committed(key)
        .map_err(mutation_error)?
        .withdraw())
}

/// Replace a committed value and return the old value.
pub fn swap<K: Eq, V, const N: usize>(
    index: &Index<K, V, N>,
    key: &K,
    replacement: V,
) -> Result<V, MutationError> {
    Ok(index
        .reserve_committed(key)
        .map_err(mutation_error)?
        .swap(replacement))
}

fn mutation_error(error: IndexError) -> MutationError {
    match error {
        IndexError::Full => MutationError::Full,
        IndexError::Duplicate => MutationError::AlreadyPresent,
        IndexError::Missing => MutationError::Missing,
        IndexError::Busy => MutationError::Busy,
    }
}
