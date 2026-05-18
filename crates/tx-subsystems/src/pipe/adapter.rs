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
//!   `tx_substrate::zone` allocation, and `tx_substrate::SpinMutex` as
//!   named pipe-side verbs (`done_bytes`, `eagain`, `epipe`,
//!   `yield_until_readable`, `yield_until_writable`, `sign`).
//!
//! * **`wait_routing`** — wraps `tx_substrate::wake::WaitSource` (v3
//!   path) and `tx_reactor::wait::{Channel, Mask}` (D2 legacy
//!   coexistence path) as named pipe-side verbs (`new_wait_source`,
//!   `fire_legacy_channel`, `notify_v3_source`). The two
//!   `#[platform_adapter]` attributes stack on the one module because
//!   pipe's wakeup routing is genuinely cross-platform and splitting
//!   would fragment the semantic unit.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone"],
    reason = "expose pipe step outcomes (done/eagain/epipe/yield) as named verbs; bundle zone allocation into pipe-domain helpers"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity, Dead, Entity,
        IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy, OperationalCapExt,
        OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy, RetainedEntityPolicy, Weak,
        Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;

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
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as pipe-side legacy wakeup verbs (PR-3D-1 D2 coexistence path)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{
        MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
    };

    /// Build a `WaitSource` for one side of a pipe, keyed by the
    /// wait-source id minted from
    /// `crate::wait_source::register_wait_channel` so the legacy
    /// (`Channel`) and v3 (`WaitSource`) paths share an id namespace
    /// (PR-3D-1 / D2 coexistence).
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

    /// Fire the legacy `Channel` for one side of a pipe — the D2
    /// coexistence path that the resolver still uses today.
    ///
    /// Delegates to `tx_reactor::wait::fire_legacy`.
    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        tx_reactor::wait::fire_legacy(channel, mask_bits)
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

    /// Notify the v3 `WaitSource` for one side of a pipe — the
    /// mailbox path that the new caller stack uses.
    ///
    /// Delegates to `tx_substrate::wake::notify`.
    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}
