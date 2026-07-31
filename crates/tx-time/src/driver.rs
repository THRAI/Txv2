//! Reactor-facing time driver capability.

/// Current-hart hardware deadline programming capability.
pub trait CurrentHartDeadlineTimer {
    fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64);
    fn cancel_current_hart_deadline(&mut self);
}

/// One requested current-hart hardware deadline transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurrentHartDeadlineAction {
    Arm { deadline_ns: u64 },
    Cancel,
}

impl CurrentHartDeadlineAction {
    pub const fn from_next_deadline(next_deadline_ns: Option<u64>) -> Self {
        match next_deadline_ns {
            Some(deadline_ns) => Self::Arm { deadline_ns },
            None => Self::Cancel,
        }
    }

    pub const fn next_deadline_ns(self) -> Option<u64> {
        match self {
            Self::Arm { deadline_ns } => Some(deadline_ns),
            Self::Cancel => None,
        }
    }

    pub fn program<T>(self, timer: &mut T)
    where
        T: CurrentHartDeadlineTimer,
    {
        match self {
            Self::Arm { deadline_ns } => timer.set_current_hart_deadline_ns(deadline_ns),
            Self::Cancel => timer.cancel_current_hart_deadline(),
        }
    }
}
