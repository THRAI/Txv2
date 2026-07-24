//! Substrate / reactor adapter for pipe.
//!
//! The two `#[platform_adapter]`-marked modules below are the legitimate
//! entry points between pipe's step semantics and the platform crates.
//! Any `tx_substrate::*` or `tx_reactor::*` token outside this file is
//! a boundary violation (caught by `cargo xtask boundary-report`).
//!
//! Two domains:
//!
//! * **`step_engine`** — wraps `tx_substrate::step` step outcomes,
//!   `tx_substrate::zone` allocation, and the subsystem lock facade as
//!   named pipe-side verbs (`done_bytes`, `eagain`, `epipe`,
//!   `yield_until_readable`, `yield_until_writable`, `sign`).
//!
//! * **`wait_routing`** — wraps `tx_substrate::wake::WaitSource` as named
//!   pipe-side verbs (`new_wait_source`, `notify_source_with_post`).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "page_allocator"],
    reason = "expose pipe step outcomes (done/eagain/epipe/yield) as named verbs; bundle zone allocation and page-backed lease frame reads into pipe-domain helpers"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, Dead, Entity, IdentRef, OperationalCapExt, PayloadCap,
        Weak, Zone, ZoneAllocated, ZoneError,
    };

    pub type ByteOutcome = StepOutcome<usize, ByteProgress>;

    /// `read` / `write` finished — `n` bytes copied. Short-read /
    /// short-write shape (`n` may be < the requested length).
    pub fn done_bytes(n: usize) -> ByteOutcome {
        StepOutcome::done(n)
    }

    /// Non-blocking pipe + no progress made → `EAGAIN`. Pipe-side
    /// verb so call sites read as the pipe spec, not as a generic
    /// errno return.
    pub fn eagain() -> ByteOutcome {
        StepOutcome::err(Errno::EAGAIN)
    }

    /// All readers gone on `write` → `EPIPE`. SIGPIPE delivery is
    /// the caller's responsibility (pipe doesn't hold a process Cap).
    pub fn epipe() -> ByteOutcome {
        StepOutcome::err(Errno::EPIPE)
    }

    /// Pipe empty and writers alive → park on the reader-side wait
    /// source. Yields `OnWaitSource { id, mask }`; consumer's
    /// `WaitFuture` resolves when the writer side fires.
    pub fn yield_until_readable(reader_wait_source_id: u64, mask: u64) -> ByteOutcome {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, reader_wait_source_id, mask)
    }

    /// Pipe full and readers alive → park on the writer-side wait
    /// source. Counterpart of `yield_until_readable`.
    pub fn yield_until_writable(writer_wait_source_id: u64, mask: u64) -> ByteOutcome {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, writer_wait_source_id, mask)
    }

    /// Bootstrap-side zone registration. Called once during workspace
    /// init via `pipe::register_zones`. The underlying substrate verb
    /// returns a `ZoneInfo` descriptor the caller has never needed;
    /// the pipe-domain wrapper drops it to keep the bootstrap surface
    /// simple.
    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        tx_substrate::zone::register_zone_for::<T>()?;
        Ok(())
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap WaitSource registration and v3 mailbox notify in pipe-side reader/writer wakeup verbs"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource};

    /// Build a `WaitSource` for one side of a pipe, keyed by the wait-source id
    /// that pipe steps stamp into `YieldShape::OnWaitSource`.
    ///
    /// Delegates to `tx_substrate::wake::new_source`. Also registers
    /// the source in the global registry so the driver can look it up
    /// by [`WaitSourceId`] during yield resolution (mirrors
    /// `vfs::adapter::wait_routing::new_wait_source`).
    pub fn new_wait_source(side_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(side_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    /// Remove a source from the global registry. Companion of
    /// `new_wait_source`; called from `PipePayload::drop` so the
    /// registry does not hold stale entries after the pipe is gone.
    ///
    /// Delegates to `tx_substrate::wake::unregister_source`.
    pub fn unregister_source(id: u64) {
        use tx_substrate::step::WaitSourceId;
        tx_substrate::wake::unregister_source(WaitSourceId::new(id));
    }

    /// Notify the `WaitSource` through a caller-provided mailbox post.
    ///
    /// This is the scheduler-context bridge used by syscall/reactor callers:
    /// the pipe subsystem still owns readiness, while the caller decides how
    /// an upgraded mailbox becomes runnable.
    pub fn notify_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        );
    }
}
