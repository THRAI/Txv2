//! Page-completion application helpers for the staged L4 service path.

use crate::io_manager::page::{PageIoCompletion, PageIoCompletionKind, PageIoResult};
use crate::page_backed::{PageSlot, PageSlotCompletionError, PageSlotSnapshot};
use tx_hal::Ppn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCompletionApplyError {
    UnexpectedKind(PageIoCompletionKind),
    MultiPageRange { page_count: u64 },
    Slot(PageSlotCompletionError),
}

impl From<PageSlotCompletionError> for PageCompletionApplyError {
    fn from(error: PageSlotCompletionError) -> Self {
        Self::Slot(error)
    }
}

pub fn apply_read_completion(
    slot: &PageSlot,
    completion: &PageIoCompletion,
    ppn: Ppn,
) -> Result<PageSlotSnapshot, PageCompletionApplyError> {
    if completion.kind != PageIoCompletionKind::ReadInstalled {
        return Err(PageCompletionApplyError::UnexpectedKind(completion.kind));
    }
    if completion.range.page_count() != 1 {
        return Err(PageCompletionApplyError::MultiPageRange {
            page_count: completion.range.page_count(),
        });
    }

    let result = match completion.result {
        PageIoResult::Done => Ok(ppn),
        PageIoResult::Err(errno) => Err(errno),
    };
    slot.complete_fetch(completion.generation, result)
        .map_err(PageCompletionApplyError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_manager::page::{
        PageIoCompletion, PageIoCompletionKind, PageIoRange, PageIoRequestId, PageIoResult,
    };
    use crate::page_backed::{PageSlot, PageSlotCompletionError, PageSlotFetch, PageSlotState};
    use tx_hal::Ppn;

    #[test]
    fn page_completion_installs_read_only_when_generation_matches() {
        let slot = PageSlot::new();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("first fetch must own the slot");
        };
        let completion = PageIoCompletion::new(
            PageIoRequestId::new(7),
            PageIoRange::new(3, 1),
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::ReadInstalled,
        );

        let snapshot =
            apply_read_completion(&slot, &completion, Ppn(42)).expect("matching completion");

        assert_eq!(snapshot.state, PageSlotState::Resident { ppn: Ppn(42) });
        assert_eq!(snapshot.generation, generation);
    }

    #[test]
    fn page_completion_rejects_stale_generation_before_installing_frame() {
        let slot = PageSlot::new();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("first fetch must own the slot");
        };
        slot.invalidate();
        let completion = PageIoCompletion::new(
            PageIoRequestId::new(8),
            PageIoRange::new(3, 1),
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::ReadInstalled,
        );

        let error = apply_read_completion(&slot, &completion, Ppn(43))
            .expect_err("stale completion must be rejected");

        assert!(matches!(
            error,
            PageCompletionApplyError::Slot(PageSlotCompletionError::GenerationMismatch {
                completed,
                ..
            }) if completed == generation
        ));
        assert_eq!(slot.snapshot().state, PageSlotState::Empty);
    }
}
