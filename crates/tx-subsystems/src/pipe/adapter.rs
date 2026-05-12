//! Substrate / reactor adapter for pipe.
//!
//! The two `#[platform_adapter]`-marked modules below are the legitimate
//! entry points between pipe's step semantics and the platform crates.
//! Any `tx_substrate::*` or `tx_reactor::*` token outside this file is
//! a boundary violation (caught by `cargo xtask boundary-report`).
//!
//! Two domains:
//!
//! * **`step_engine`** — wraps `tx_substrate::step_v3` step outcomes,
//!   `tx_substrate::zone` allocation, and `tx_substrate::SpinMutex` as
//!   named pipe-side verbs (`done_bytes`, `eagain`, `epipe`,
//!   `yield_until_readable`, `yield_until_writable`, `sign_zone_for`).
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
    apis = ["step_v3", "zone"],
    reason = "expose pipe step outcomes (done/eagain/epipe/yield) as named verbs; bundle zone allocation into pipe-domain helpers"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        ByteProgress, Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp,
        StepOutcome, StepProgress, SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{Cap, Zone, ZoneAllocated, ZoneError};
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

    /// Reserve + sign in one step: mint a `Cap<T>` from `T`'s zone.
    /// Fails iff the zone is exhausted; `step_pipe2` maps that to
    /// `Errno::ENOMEM`.
    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }

    /// Bootstrap-side zone registration. Called once during workspace
    /// init via `pipe::register_zones`. The underlying substrate verb
    /// returns a `ZoneInfo` descriptor the caller has never needed;
    /// the pipe-domain wrapper drops it to keep the bootstrap surface
    /// simple.
    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        zone::register_zone_for::<T>()?;
        Ok(())
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step_v3"],
    reason = "wrap WaitSource registration and v3 mailbox notify in pipe-side reader/writer wakeup verbs"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as pipe-side legacy wakeup verbs (PR-3D-1 D2 coexistence path)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;
    use tx_substrate::step_v3::{InterestMask, WaitSourceId};

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource};

    /// Build a `WaitSource` for one side of a pipe, keyed by the
    /// wait-source id minted from
    /// `crate::wait_source::register_wait_channel` so the legacy
    /// (`Channel`) and v3 (`WaitSource`) paths share an id namespace
    /// (PR-3D-1 / D2 coexistence).
    pub fn new_wait_source(side_id: u64) -> Arc<WaitSource> {
        Arc::new(WaitSource::new(WaitSourceId::new(side_id)))
    }

    /// Fire the legacy `Channel` for one side of a pipe — the D2
    /// coexistence path that the resolver still uses today.
    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        channel.fire(Mask::from_bits(mask_bits))
    }

    /// Notify the v3 `WaitSource` for one side of a pipe — the
    /// mailbox path that the new caller stack uses.
    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        source.notify(InterestMask::new(mask_bits));
    }
}
