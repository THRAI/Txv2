//! One-shot fsync graph submission state.

use crate::execution::Errno;
use crate::io_manager::page::PageIoRequestId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsyncSubmissionState {
    Queued,
    Complete(Result<(), Errno>),
    Consumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsyncSubmission {
    id: PageIoRequestId,
    state: FsyncSubmissionState,
}

impl FsyncSubmission {
    pub const fn new(id: PageIoRequestId) -> Self {
        Self {
            id,
            state: FsyncSubmissionState::Queued,
        }
    }
    pub const fn id(self) -> PageIoRequestId {
        self.id
    }
    pub const fn state(self) -> FsyncSubmissionState {
        self.state
    }
    pub fn complete(&mut self, result: Result<(), Errno>) -> bool {
        if matches!(self.state, FsyncSubmissionState::Queued) {
            self.state = FsyncSubmissionState::Complete(result);
            true
        } else {
            false
        }
    }
    pub fn take(&mut self) -> Option<Result<(), Errno>> {
        let FsyncSubmissionState::Complete(result) = self.state else {
            return None;
        };
        self.state = FsyncSubmissionState::Consumed;
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_result_is_consumed_once() {
        let mut row = FsyncSubmission::new(PageIoRequestId::new(7));
        assert!(row.complete(Ok(())));
        assert!(!row.complete(Err(Errno::EIO)));
        assert_eq!(row.take(), Some(Ok(())));
        assert_eq!(row.take(), None);
    }
}
