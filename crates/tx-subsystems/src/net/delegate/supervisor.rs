use smoltcp::time::Instant;
use tx_reactor::wait::WaitOutcome;
use tx_services::time::DeadlineRegistrar;

use super::{net_delegate_wait_tick_deadline, smoltcp_instant_to_reactor_deadline_ns};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetDelegateTimerArm {
    pub generation: u64,
    pub deadline: Instant,
    pub deadline_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetDelegateTimerWake {
    pub generation: u64,
    pub outcome: WaitOutcome,
    pub tick_fired: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetDelegateSupervisor {
    smoltcp_base: Instant,
    reactor_base_ns: u64,
    generation: u64,
    armed: Option<NetDelegateTimerArm>,
}

impl NetDelegateSupervisor {
    pub const fn new(smoltcp_base: Instant, reactor_base_ns: u64) -> Self {
        Self {
            smoltcp_base,
            reactor_base_ns,
            generation: 0,
            armed: None,
        }
    }

    pub const fn armed(&self) -> Option<NetDelegateTimerArm> {
        self.armed
    }

    pub fn refresh_deadline(
        &mut self,
        next_deadline: Option<Instant>,
    ) -> Option<NetDelegateTimerArm> {
        let Some(deadline) = next_deadline else {
            if self.armed.is_some() {
                self.generation = self.generation.wrapping_add(1);
                self.armed = None;
            }
            return None;
        };
        if self.armed.is_some_and(|arm| arm.deadline == deadline) {
            return None;
        }

        let deadline_ns = smoltcp_instant_to_reactor_deadline_ns(
            self.smoltcp_base,
            deadline,
            self.reactor_base_ns,
        )?;
        self.generation = self.generation.wrapping_add(1);
        let arm = NetDelegateTimerArm {
            generation: self.generation,
            deadline,
            deadline_ns,
        };
        self.armed = Some(arm);
        Some(arm)
    }

    pub fn accept_wake(&self, wake: NetDelegateTimerWake) -> bool {
        self.armed
            .is_some_and(|arm| arm.generation == wake.generation && wake.tick_fired)
    }

    pub fn consume_wake(&mut self, wake: NetDelegateTimerWake) -> bool {
        if self
            .armed
            .is_some_and(|arm| arm.generation == wake.generation && wake.tick_fired)
        {
            self.generation = self.generation.wrapping_add(1);
            self.armed = None;
            true
        } else {
            false
        }
    }
}

pub async fn net_delegate_wait_supervised_deadline<R>(
    timer_registrar: &R,
    arm: NetDelegateTimerArm,
) -> NetDelegateTimerWake
where
    R: DeadlineRegistrar + ?Sized,
{
    let outcome = net_delegate_wait_tick_deadline(timer_registrar, arm.deadline_ns).await;
    NetDelegateTimerWake {
        generation: arm.generation,
        outcome,
        tick_fired: matches!(outcome, WaitOutcome::TimedOut),
    }
}
