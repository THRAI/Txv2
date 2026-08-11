//! Page-level state staging for the future I/O manager miss/completion path.
//!
//! This module is intentionally detached from the live `PageContainer`
//! materialization path for now. It fixes the PageBacked-owned state machine
//! shape that L4 completions will later use: deduplicated fetch ownership,
//! generation-checked completion, and per-page dirty/writeback transitions.

use core::fmt;

use crate::execution::Errno;
use crate::io_manager::page::PageGeneration;
use crate::sync::SpinMutex;
use tx_hal::Ppn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSlotState {
    Empty,
    Resident {
        ppn: Ppn,
    },
    Fetching,
    Dirty {
        ppn: Ppn,
    },
    Writeback {
        ppn: Ppn,
        submitted_generation: PageGeneration,
        redirtied: bool,
    },
    Error {
        errno: Errno,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageSlotSnapshot {
    pub state: PageSlotState,
    pub generation: PageGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSlotFetch {
    Owner {
        generation: PageGeneration,
    },
    Joined {
        generation: PageGeneration,
    },
    Resident {
        ppn: Ppn,
        generation: PageGeneration,
    },
    Blocked {
        state: PageSlotState,
        generation: PageGeneration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSlotCompletionError {
    GenerationMismatch {
        current: PageGeneration,
        completed: PageGeneration,
    },
    MismatchedFrame {
        expected: Ppn,
        current: Ppn,
    },
    NotFetching {
        state: PageSlotState,
        generation: PageGeneration,
    },
    Backend(Errno),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSlotFsyncStatus {
    Clean,
    NeedsWriteback {
        generation: PageGeneration,
    },
    WaitingForWriteback {
        submitted_generation: PageGeneration,
    },
    WaitingForEarlierWriteback {
        submitted_generation: PageGeneration,
        frontier: PageGeneration,
    },
    Error {
        errno: Errno,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageSlotInner {
    state: PageSlotState,
    generation: PageGeneration,
}

impl PageSlotInner {
    const fn empty() -> Self {
        Self {
            state: PageSlotState::Empty,
            generation: PageGeneration::new(0),
        }
    }

    fn bump_generation(&mut self) -> PageGeneration {
        let next = self.generation.raw().wrapping_add(1).max(1);
        self.generation = PageGeneration::new(next);
        self.generation
    }

    const fn snapshot(&self) -> PageSlotSnapshot {
        PageSlotSnapshot {
            state: self.state,
            generation: self.generation,
        }
    }
}

pub struct PageSlot {
    inner: SpinMutex<PageSlotInner>,
}

impl PageSlot {
    pub const fn new() -> Self {
        Self {
            inner: SpinMutex::new(PageSlotInner::empty()),
        }
    }

    pub fn snapshot(&self) -> PageSlotSnapshot {
        self.inner.lock().snapshot()
    }

    pub fn generation(&self) -> PageGeneration {
        self.inner.lock().generation
    }

    /// Classify this page against an fsync entry frontier without submitting I/O.
    pub fn fsync_status(&self, frontier: PageGeneration) -> PageSlotFsyncStatus {
        let inner = self.inner.lock();
        match inner.state {
            PageSlotState::Resident { .. } | PageSlotState::Empty | PageSlotState::Fetching => {
                PageSlotFsyncStatus::Clean
            }
            PageSlotState::Dirty { .. } => {
                if inner.generation <= frontier {
                    PageSlotFsyncStatus::NeedsWriteback {
                        generation: inner.generation,
                    }
                } else {
                    PageSlotFsyncStatus::Clean
                }
            }
            PageSlotState::Writeback {
                submitted_generation,
                ..
            } if submitted_generation >= frontier => PageSlotFsyncStatus::WaitingForWriteback {
                submitted_generation,
            },
            PageSlotState::Writeback {
                submitted_generation,
                ..
            } => PageSlotFsyncStatus::WaitingForEarlierWriteback {
                submitted_generation,
                frontier,
            },
            PageSlotState::Error { errno } => PageSlotFsyncStatus::Error { errno },
        }
    }

    pub fn begin_fetch(&self) -> PageSlotFetch {
        let mut inner = self.inner.lock();
        match inner.state {
            PageSlotState::Empty | PageSlotState::Error { .. } => {
                let generation = inner.bump_generation();
                inner.state = PageSlotState::Fetching;
                PageSlotFetch::Owner { generation }
            }
            PageSlotState::Fetching => PageSlotFetch::Joined {
                generation: inner.generation,
            },
            PageSlotState::Resident { ppn } | PageSlotState::Dirty { ppn } => {
                PageSlotFetch::Resident {
                    ppn,
                    generation: inner.generation,
                }
            }
            PageSlotState::Writeback { .. } => PageSlotFetch::Blocked {
                state: inner.state,
                generation: inner.generation,
            },
        }
    }

    pub fn complete_fetch(
        &self,
        generation: PageGeneration,
        result: Result<Ppn, Errno>,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        Self::ensure_generation(&inner, generation)?;
        if inner.state != PageSlotState::Fetching {
            return Err(PageSlotCompletionError::NotFetching {
                state: inner.state,
                generation: inner.generation,
            });
        }

        inner.state = match result {
            Ok(ppn) => PageSlotState::Resident { ppn },
            Err(errno) => PageSlotState::Error { errno },
        };
        Ok(inner.snapshot())
    }

    pub fn mark_dirty(&self) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        match inner.state {
            PageSlotState::Resident { ppn } | PageSlotState::Dirty { ppn } => {
                inner.bump_generation();
                inner.state = PageSlotState::Dirty { ppn };
                Ok(inner.snapshot())
            }
            PageSlotState::Writeback {
                ppn,
                submitted_generation,
                ..
            } => {
                inner.bump_generation();
                inner.state = PageSlotState::Writeback {
                    ppn,
                    submitted_generation,
                    redirtied: true,
                };
                Ok(inner.snapshot())
            }
            state => Err(PageSlotCompletionError::NotFetching {
                state,
                generation: inner.generation,
            }),
        }
    }

    pub fn begin_writeback(&self) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        match inner.state {
            PageSlotState::Dirty { ppn } | PageSlotState::Resident { ppn } => {
                inner.state = PageSlotState::Writeback {
                    ppn,
                    submitted_generation: inner.generation,
                    redirtied: false,
                };
                Ok(inner.snapshot())
            }
            state => Err(PageSlotCompletionError::NotFetching {
                state,
                generation: inner.generation,
            }),
        }
    }

    pub fn complete_writeback(
        &self,
        generation: PageGeneration,
        result: Result<(), Errno>,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        let PageSlotState::Writeback {
            ppn,
            submitted_generation,
            redirtied,
        } = inner.state
        else {
            return Err(PageSlotCompletionError::NotFetching {
                state: inner.state,
                generation: inner.generation,
            });
        };

        if submitted_generation != generation {
            return Err(PageSlotCompletionError::GenerationMismatch {
                current: inner.generation,
                completed: generation,
            });
        }

        match result {
            Ok(()) if redirtied => inner.state = PageSlotState::Dirty { ppn },
            Ok(()) => inner.state = PageSlotState::Resident { ppn },
            Err(_) if redirtied => inner.state = PageSlotState::Dirty { ppn },
            Err(errno) => inner.state = PageSlotState::Error { errno },
        }
        Ok(inner.snapshot())
    }

    /// Restore a writeback that was never handed to a backend executor.
    pub fn abort_writeback(
        &self,
        generation: PageGeneration,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        let PageSlotState::Writeback {
            ppn,
            submitted_generation,
            ..
        } = inner.state
        else {
            return Err(PageSlotCompletionError::NotFetching {
                state: inner.state,
                generation: inner.generation,
            });
        };
        if submitted_generation != generation {
            return Err(PageSlotCompletionError::GenerationMismatch {
                current: inner.generation,
                completed: generation,
            });
        }

        inner.state = PageSlotState::Dirty { ppn };
        Ok(inner.snapshot())
    }

    pub fn invalidate(&self) -> PageSlotSnapshot {
        let mut inner = self.inner.lock();
        inner.bump_generation();
        inner.state = PageSlotState::Empty;
        inner.snapshot()
    }

    /// Withdraw the exact resident binding before its cache entry is removed.
    ///
    /// The caller supplies the binding generation and PPN observed while it
    /// still owns the PageContainer mutation boundary. A stale completion or a
    /// replacement binding therefore cannot be invalidated by an older owner.
    pub fn withdraw_if_matches(
        &self,
        expected_generation: PageGeneration,
        expected_ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        Self::ensure_generation(&inner, expected_generation)?;
        let current = match inner.state {
            PageSlotState::Resident { ppn }
            | PageSlotState::Dirty { ppn }
            | PageSlotState::Writeback { ppn, .. } => ppn,
            state => {
                return Err(PageSlotCompletionError::NotFetching {
                    state,
                    generation: inner.generation,
                });
            }
        };
        if current != expected_ppn {
            return Err(PageSlotCompletionError::MismatchedFrame {
                expected: expected_ppn,
                current,
            });
        }

        inner.bump_generation();
        inner.state = PageSlotState::Empty;
        Ok(inner.snapshot())
    }

    /// Rebind an exact non-writeback resident generation to a replacement
    /// frame. CoW uses this while holding the PageContainer mutation boundary:
    /// an in-flight writeback cannot be retargeted because its completion owns
    /// the old frame and generation.
    pub fn replace_if_matches(
        &self,
        expected_generation: PageGeneration,
        expected_ppn: Ppn,
        replacement_ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        Self::ensure_generation(&inner, expected_generation)?;
        let replacement = match inner.state {
            PageSlotState::Resident { ppn } if ppn == expected_ppn => PageSlotState::Resident {
                ppn: replacement_ppn,
            },
            PageSlotState::Dirty { ppn } if ppn == expected_ppn => PageSlotState::Dirty {
                ppn: replacement_ppn,
            },
            PageSlotState::Resident { ppn } | PageSlotState::Dirty { ppn } => {
                return Err(PageSlotCompletionError::MismatchedFrame {
                    expected: expected_ppn,
                    current: ppn,
                });
            }
            state @ PageSlotState::Writeback { .. } => {
                return Err(PageSlotCompletionError::NotFetching {
                    state,
                    generation: inner.generation,
                });
            }
            state => {
                return Err(PageSlotCompletionError::NotFetching {
                    state,
                    generation: inner.generation,
                });
            }
        };
        inner.bump_generation();
        inner.state = replacement;
        Ok(inner.snapshot())
    }

    /// Set one dirty generation clean only when the matching resident binding
    /// has not been redirtied or replaced since the caller captured it.
    pub fn mark_clean_if_matches(
        &self,
        expected_generation: PageGeneration,
        expected_ppn: Ppn,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        Self::ensure_generation(&inner, expected_generation)?;
        let PageSlotState::Dirty { ppn: current } = inner.state else {
            return Err(PageSlotCompletionError::NotFetching {
                state: inner.state,
                generation: inner.generation,
            });
        };
        if current != expected_ppn {
            return Err(PageSlotCompletionError::MismatchedFrame {
                expected: expected_ppn,
                current,
            });
        }
        inner.state = PageSlotState::Resident { ppn: current };
        Ok(inner.snapshot())
    }

    /// Cancel a fetch only if it is still the generation the caller owns.
    pub fn invalidate_if_generation(
        &self,
        expected_generation: PageGeneration,
    ) -> Result<PageSlotSnapshot, PageSlotCompletionError> {
        let mut inner = self.inner.lock();
        Self::ensure_generation(&inner, expected_generation)?;
        inner.bump_generation();
        inner.state = PageSlotState::Empty;
        Ok(inner.snapshot())
    }

    fn ensure_generation(
        inner: &PageSlotInner,
        completed: PageGeneration,
    ) -> Result<(), PageSlotCompletionError> {
        if inner.generation == completed {
            Ok(())
        } else {
            Err(PageSlotCompletionError::GenerationMismatch {
                current: inner.generation,
                completed,
            })
        }
    }
}

impl Default for PageSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PageSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageSlot")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}
