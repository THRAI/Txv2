//! Mutation helpers layered over index reservation/commit states.

use crate::index::{CommittedReservation, Index, IndexError};

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

/// A committed entry reserved for conditional withdrawal.
///
/// Dropping this value restores the committed entry; [`Self::withdraw`]
/// commits the removal. This lets callers reserve several related entries
/// before making any of the removals visible.
#[must_use]
pub struct WithdrawReservation<'i, K, V, const N: usize> {
    inner: CommittedReservation<'i, K, V, N>,
}

impl<K, V, const N: usize> WithdrawReservation<'_, K, V, N> {
    pub fn value(&self) -> &V {
        self.inner.value()
    }

    pub fn withdraw(self) -> V {
        self.inner.withdraw()
    }
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

/// Withdraw a committed value only while it still satisfies `predicate`.
pub fn withdraw_if<K: Eq, V, const N: usize>(
    index: &Index<K, V, N>,
    key: &K,
    predicate: impl FnOnce(&V) -> bool,
) -> Result<Option<V>, MutationError> {
    Ok(reserve_withdraw_if(index, key, predicate)?.map(WithdrawReservation::withdraw))
}

/// Reserve a committed value for withdrawal only while it still satisfies
/// `predicate`. A rejected predicate leaves the entry unchanged.
pub fn reserve_withdraw_if<'i, K: Eq, V, const N: usize>(
    index: &'i Index<K, V, N>,
    key: &K,
    predicate: impl FnOnce(&V) -> bool,
) -> Result<Option<WithdrawReservation<'i, K, V, N>>, MutationError> {
    let reservation = index.reserve_committed(key).map_err(mutation_error)?;
    if !reservation.value_matches(predicate) {
        return Ok(None);
    }
    Ok(Some(WithdrawReservation { inner: reservation }))
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
