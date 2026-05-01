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

pub mod ast;
pub mod completion;
pub mod dispatch;
pub mod hart_loop;
pub mod interrupt;
pub mod preempt;
mod runtime;
pub mod scheduler;
pub(crate) mod spin_lock;
pub mod sync_coord;
pub mod task;
pub(crate) mod timer;
pub mod userspace;
pub mod wait;
pub(crate) mod waker;
mod yield_now;

pub use dispatch::{
    DispatchState, NoopRescheduleSignal, RescheduleSignal, WakeDispatchAction, WakeDispatchReport,
};
pub use runtime::{Reactor, RunIdleReport, RunStats, SharedReactor};
pub use scheduler::{
    HartId, InitialSchedMeta, Phase1Scheduler, RunnablePlacement, SchedClass, SchedulerPolicy,
    SliceConfig, StopReason, TaskHandle, WakeHint,
};
pub use task::{TaskDrainRecord, TaskId, TaskKey, TaskLifecycleError, TaskStatus};
pub use yield_now::{yield_now, YieldNow};
