use crate::execution::{Errno, Guard};
use crate::page_backed::{ErrorCursor, ErrorSeq, FileFsyncFrontier};
use crate::vfs::adapter::step_engine::{NoProgress, StepOutcome};
use crate::vfs::FsObjectId;

use super::MountPayloadPin;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountRuntimeState {
    Open,
    Quiescing,
    RecoveryOnly,
    DetachedPending,
    Detached,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountTransactionFrontier(u64);

impl MountTransactionFrontier {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementScope {
    File {
        object: FsObjectId,
        generation_frontier: FileFsyncFrontier,
    },
    Mount {
        transaction_frontier: MountTransactionFrontier,
    },
    Detach,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementPhase {
    Idle,
    Active(SettlementScope),
}

#[derive(Debug)]
pub struct MountRuntimeCell {
    state: MountRuntimeState,
    active_payload_users: u32,
    active_settlement: SettlementPhase,
    error_seq: ErrorSeq,
    mount_cursor: ErrorCursor,
    payload_cursor: ErrorCursor,
}

pub struct MountSettlementOp {
    payload: MountPayloadPin,
    scope: SettlementScope,
    phase: MountSettlementPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MountSettlementPhase {
    Ready,
    Complete,
}

impl MountRuntimeCell {
    pub const fn new() -> Self {
        Self {
            state: MountRuntimeState::Open,
            active_payload_users: 0,
            active_settlement: SettlementPhase::Idle,
            error_seq: ErrorSeq::new(),
            mount_cursor: ErrorCursor::new(),
            payload_cursor: ErrorCursor::new(),
        }
    }

    pub fn state(&self) -> MountRuntimeState {
        self.state
    }

    pub fn note_payload_pin_acquired(&mut self) {
        self.active_payload_users = self.active_payload_users.saturating_add(1);
    }

    pub fn note_payload_pin_released(&mut self) -> bool {
        self.active_payload_users = self.active_payload_users.saturating_sub(1);
        if self.state == MountRuntimeState::DetachedPending && self.active_payload_users == 0 {
            self.state = MountRuntimeState::Quiescing;
            return true;
        }
        false
    }

    pub fn begin_lazy_detach(&mut self) -> Result<bool, Errno> {
        match self.state {
            MountRuntimeState::Open => {
                if self.active_payload_users == 0 {
                    self.state = MountRuntimeState::Quiescing;
                    Ok(true)
                } else {
                    self.state = MountRuntimeState::DetachedPending;
                    Ok(false)
                }
            }
            MountRuntimeState::DetachedPending | MountRuntimeState::Detached => Err(Errno::EBUSY),
            MountRuntimeState::Quiescing | MountRuntimeState::RecoveryOnly => Ok(true),
        }
    }

    pub fn try_claim_settlement(&mut self, scope: SettlementScope) -> Result<(), Errno> {
        if !matches!(self.active_settlement, SettlementPhase::Idle) {
            return Err(Errno::EBUSY);
        }
        self.active_settlement = SettlementPhase::Active(scope.clone());
        match scope {
            SettlementScope::Detach => {
                if self.state == MountRuntimeState::Open {
                    self.state = MountRuntimeState::Quiescing;
                }
            }
            SettlementScope::Mount { .. } => {
                if self.state == MountRuntimeState::Open {
                    self.state = MountRuntimeState::Quiescing;
                }
            }
            SettlementScope::File { .. } => {}
        }
        Ok(())
    }

    pub fn complete_settlement(&mut self, result: Result<(), Errno>) {
        if let Err(errno) = result {
            self.error_seq.record(errno);
            self.state = MountRuntimeState::RecoveryOnly;
        } else if matches!(
            &self.active_settlement,
            SettlementPhase::Active(SettlementScope::Detach)
        ) && self.state != MountRuntimeState::DetachedPending
        {
            self.state = MountRuntimeState::Detached;
        } else if self.state == MountRuntimeState::Quiescing {
            self.state = MountRuntimeState::Open;
        }
        self.active_settlement = SettlementPhase::Idle;
    }

    pub fn observe_mount_error(&mut self) -> Option<Errno> {
        self.error_seq.observe(&mut self.mount_cursor)
    }

    pub fn observe_payload_error(&mut self) -> Option<Errno> {
        self.error_seq.observe(&mut self.payload_cursor)
    }

    pub fn snapshot_error_cursor(&self) -> ErrorCursor {
        self.error_seq.snapshot_cursor()
    }

    pub fn observe_mount_error_with_cursor(&self, cursor: &mut ErrorCursor) -> Option<Errno> {
        self.error_seq.observe(cursor)
    }

    pub fn observe_payload_error_with_cursor(&self, cursor: &mut ErrorCursor) -> Option<Errno> {
        self.error_seq.observe(cursor)
    }
}

impl MountSettlementOp {
    pub fn new(payload: MountPayloadPin, scope: SettlementScope) -> Result<Self, Errno> {
        payload.payload().try_claim_settlement(scope.clone())?;
        Ok(Self {
            payload,
            scope,
            phase: MountSettlementPhase::Ready,
        })
    }

    pub fn payload_pin_count(&self) -> u32 {
        self.payload.payload().payload_pin_count()
    }

    pub fn drive(&mut self, guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if self.phase == MountSettlementPhase::Complete {
            return StepOutcome::done(());
        }

        let result = match self.scope {
            SettlementScope::Detach => match self.payload.payload().fs_ops().shutdown(guard) {
                StepOutcome::Done(()) => Ok(()),
                StepOutcome::Err(errno) => Err(errno.into()),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    return StepOutcome::err(Errno::EAGAIN.into());
                }
            },
            SettlementScope::File {
                object,
                ref generation_frontier,
            } => match self.payload.payload().fs_ops().settle_file(
                object,
                generation_frontier,
                guard,
            ) {
                StepOutcome::Done(()) => Ok(()),
                StepOutcome::Err(errno) => Err(errno.into()),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    return StepOutcome::err(Errno::EAGAIN.into());
                }
            },
            SettlementScope::Mount {
                transaction_frontier,
            } => match self
                .payload
                .payload()
                .fs_ops()
                .settle_mount(transaction_frontier, guard)
            {
                StepOutcome::Done(()) => Ok(()),
                StepOutcome::Err(errno) => Err(errno.into()),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    return StepOutcome::err(Errno::EAGAIN.into());
                }
            },
        };
        self.payload.payload().complete_settlement(result);
        self.phase = MountSettlementPhase::Complete;

        match result {
            Ok(()) => StepOutcome::done(()),
            Err(errno) => StepOutcome::err(errno.into()),
        }
    }
}

impl Default for MountRuntimeCell {
    fn default() -> Self {
        Self::new()
    }
}
