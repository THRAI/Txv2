//! Network delegate wake queue.

mod queue;
mod runtime;
mod supervisor;
mod timer;

pub(crate) use queue::net_delegate_take;
pub use queue::{
    net_delegate_carrier_id, net_delegate_clear, net_delegate_kick_poll_with_post,
    net_delegate_kick_tick_with_post, net_delegate_queue, net_delegate_wait_token, DelegateWireSet,
};
pub use runtime::{
    net_delegate_step_once, net_delegate_task_loop, net_delegate_task_loop_owned,
    net_delegate_task_loop_owned_with_deadline_hook, net_delegate_task_loop_with_deadline_hook,
    NetDelegateDriver, NetDelegateRuntimeOutcome, NetDelegateTaskConfig, NetDelegateTaskReport,
};
pub use supervisor::{
    net_delegate_wait_supervised_deadline, NetDelegateSupervisor, NetDelegateTimerArm,
    NetDelegateTimerWake,
};
pub use timer::{net_delegate_wait_tick_deadline, smoltcp_instant_to_reactor_deadline_ns};
