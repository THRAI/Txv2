use alloc::vec::Vec;

/// Sparse integer-keyed index surface for PageBacked-style caches.
///
/// This is the Tx-facing subset of Linux XArray's normal API:
///
/// - `load` mirrors `xa_load`.
/// - `insert` mirrors `xa_insert`.
/// - `erase` / `erase_from` mirror `xa_erase` and range withdrawal.
/// - `compare_replace` mirrors `xa_cmpxchg`.
/// - mark operations mirror `xa_get_mark`, `xa_set_mark`, `xa_clear_mark`,
///   and marked iteration.
///
/// The first backend is intentionally allowed to be lock-backed by its owner.
/// Later backends can make `load` guard/EBR based without changing PageBacked
/// call sites.
pub(crate) trait SparseIndex {
    type Key: Copy + Ord;
    type Entry;
    type Error;
    type Mark: Copy;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn load(&self, key: Self::Key) -> Option<&Self::Entry>;
    fn load_mut(&mut self, key: Self::Key) -> Option<&mut Self::Entry>;

    fn insert(&mut self, key: Self::Key, entry: Self::Entry) -> Result<(), Self::Error>;

    fn compare_replace(
        &mut self,
        key: Self::Key,
        matches: impl FnOnce(&Self::Entry) -> bool,
        replacement: Option<Self::Entry>,
    ) -> Result<Option<Self::Entry>, Self::Error>;

    fn erase(&mut self, key: Self::Key) -> Option<Self::Entry>;
    fn erase_from(&mut self, first: Self::Key);

    fn get_mark(&self, key: Self::Key, mark: Self::Mark) -> bool;
    fn set_mark(&mut self, key: Self::Key, mark: Self::Mark) -> Result<(), Self::Error>;
    fn clear_mark(&mut self, key: Self::Key, mark: Self::Mark) -> Result<(), Self::Error>;
    fn marked(&self, mark: Self::Mark) -> bool;
    fn collect_marked(&self, mark: Self::Mark) -> Vec<(Self::Key, &Self::Entry)>;
}
