#![no_std]
//! Minimal task-aware reactor machinery.
//!
//! This crate is still below the full REACTOR_v0 contract: the current
//! `request_userspace_run` facade is a single-slot shell without
//! `ThreadPayload`, VM, signal, or HAL return integration; AST delivery
//! policy, full cross-hart runtime integration, and the long-running
//! interrupt-driven idle loop are not implemented yet. The implemented
//! invariant is narrower and load-bearing for those later pieces: each
//! submitted task owns the wake state used by its `Waker`, so a wake marks
//! exactly that task runnable and does not authorize semantic truth.
//! `wait_event` makes that re-observation rule explicit by rechecking its
//! condition after every channel wake or timeout wake.

extern crate alloc;

pub mod adapter;
pub mod agent_reply;
pub mod ast;
pub mod completion;
mod deadline_registry;
pub mod dispatch;
pub mod hart_loop;
pub mod interrupt;
pub mod mailbox;
pub mod preempt;
mod runtime;
pub mod scheduler;
pub(crate) mod spin_lock;
pub mod sync_coord;
pub mod task;
pub mod userspace;
pub mod wait;
pub mod wait_source;
pub(crate) mod waker;
mod yield_now;

pub use agent_reply::{await_agent_reply, AgentReplyOutcome, AwaitAgentReply};
pub use dispatch::{
    DispatchState, NoopRescheduleSignal, RescheduleSignal, WakeDispatchAction, WakeDispatchReport,
};
pub use mailbox::{
    ActiveWait, MailboxEvent, SignalRouting, TaskMailbox, WaitGeneration, MAILBOX_QUEUE_BOUND,
};
pub use runtime::{
    HartPollBudget, HartRunStats, HartRuntimeView, Reactor, ReactorObservability, RunIdleReport,
    RunStats, SharedReactor, SliceClock, TaskPublishReport,
};
pub use scheduler::{
    HartId, HartSchedulerLocal, InitialSchedMeta, LocalEnqueueRequest, MigrationPolicy,
    Phase1QueueKind, Phase1Scheduler, QueuedTaskReport, RunnablePlacement, SchedClass,
    SchedulerAffinityError, SchedulerStats, SliceConfig, StopReason, TaskHandle, TaskRunOwner,
    WakeHint,
};
pub use task::{
    current_deadline_registrar, current_delegate_registry, current_task_mailbox, TaskDrainRecord,
    TaskId, TaskKey, TaskLifecycleError, TaskStatus,
};
pub use wait_source::{PreparedWaitRegistration, SubscriberId, WaitRegistrationGuard, WaitSource};
pub use yield_now::{yield_now, YieldNow};
