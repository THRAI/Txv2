//! Eager-walk user-access primitives.
//!
//! Replaces the retired `tx_hal::UserAccessIf` trait. Per HAL_v1.md
//! §12 ("retired by axhal adoption") and PAGE_BACKED_v1 §5.1 ("the
//! user buffer is materialised through its `VmEntry.backing` and
//! copied through the kernel direct-map view"), user-side copies walk
//! the user range against the AddressSpace's recipes BTree
//! page-by-page, materialise each page through its `VmEntry.backing`,
//! translate the resulting frame's PPN to a kernel direct-map
//! address, and do plain `core::ptr::copy_nonoverlapping`.
//!
//! No fixup table, no SUM dance, no asm. The fault-recovery
//! mechanism the old design specified is replaced by eager walk:
//! every materialisation yields either a frame or `Err`/`Blocked`,
//! both of which surface to the syscall caller without relying on
//! kernel-mode fault recovery.
//!
//! ## PrivateAnon consistency via pmap
//!
//! Per VM_v1_2 §"No rmap": private anon frames are tracked only via
//! published pmap PTEs (no per-VmEntry rmap). `resolve_user_page_addr`
//! therefore consults `aspace.pmap.lookup(page)` first; on hit it
//! reuses the cached `(ppn, prot)`, on miss it falls through to the
//! VmFault materialisation path. Without the pmap-first probe a
//! `PrivateAnon` page would be re-zeroed on every read (each call
//! allocates a fresh zero frame), so a write through `copy_to_user`
//! would not be visible to a subsequent `copy_from_user`. brk-backed
//! heap (musl malloc / argv strings / env strings) requires
//! cross-call consistency, hence the pmap-first lane.
//!
//! `reserve_user_range_for_access` populates pmap eagerly for a
//! whole user range so a subsequent multi-step copy sequence
//! (struct + string, struct + buffer, etc.) sees each page exactly
//! once, mirrors the canonical `fault_script` lane synchronously, and
//! returns `Blocked`/`Err` to its caller for any page whose
//! materialisation cannot complete inline.

use alloc::vec::Vec;
use step_engine::page_allocator;
use tx_hal::UserPtr;

use crate::execution::{Errno, Guard, WaitToken};
use crate::page_backed::{MaterializeAccess, MaterializedPage, PageIndex};

use super::structure::{
    AccessMode, AddressSpace, PrivateFrame, PrivateFrameState, PrivatePageError, UserRange,
    UserVirtAddr, VmEntry, VmEntryBacking, VmFault, VmFaultOutcome, USER_PAGE_SIZE,
};
use crate::vm::adapter::step_engine::{self as step_engine, ByteProgress, NoProgress, StepOutcome};
use crate::vm::checks::require_fault_recipe;
use crate::vm::LockMode;

/// Whether a user-access primitive is reading from or writing to
/// user-space memory. Determines both the protection check and the
/// materialisation access mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserAccessKind {
    Read,
    Write,
}

impl UserAccessKind {
    pub fn required_prot(self) -> AccessMode {
        match self {
            Self::Read => AccessMode::Read,
            Self::Write => AccessMode::Write,
        }
    }

    fn materialize_access(self) -> MaterializeAccess {
        match self {
            Self::Read => MaterializeAccess::Read,
            Self::Write => MaterializeAccess::Write,
        }
    }
}

impl AddressSpace {
    /// Copy `dst.len()` bytes from user-space `src` to kernel-side `dst`.
    /// Returns the number of bytes copied (= `dst.len()` on success).
    pub fn copy_from_user(
        &self,
        dst: &mut [u8],
        src: UserPtr<u8>,
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        copy_in(self, dst, src, guard)
    }

    /// Copy `src.len()` bytes from kernel-side `src` to user-space `dst`.
    /// Returns the number of bytes copied (= `src.len()` on success).
    pub fn copy_to_user(
        &self,
        dst: UserPtr<u8>,
        src: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        copy_out(self, dst, src, guard)
    }

    /// Read a `T: Copy` value from user-space `src`. Wrapper over
    /// `copy_from_user`.
    ///
    /// Returns a step_v3 outcome with `NoProgress` (one-shot read; the
    /// inner copy's byte-progress accumulator is summarised away here
    /// because a partial-`T` read is not a meaningful intermediate
    /// state for the caller). The inner v3 `copy_from_user` is bridged
    /// per-variant: `Done`/`Continue` → `Done(value)`,
    /// `Yield { shape, .. }` → `Yield { progress: NoProgress, shape }`,
    /// `Err` → `Err(errno)` (errno already in `step_v3::Errno`).
    pub fn read_user<T: Copy>(
        &self,
        src: UserPtr<T>,
        guard: &Guard<'_>,
    ) -> StepOutcome<T, NoProgress> {
        use step_engine::{NoProgress, StepOutcome as V3};
        let mut value = core::mem::MaybeUninit::<T>::uninit();
        // SAFETY: value is a valid kernel-stack `MaybeUninit<T>`; we
        // expose its bytes to copy_from_user which fills exactly
        // `size_of::<T>()` bytes before we call `assume_init`.
        let dst_bytes = unsafe {
            core::slice::from_raw_parts_mut(
                value.as_mut_ptr() as *mut u8,
                core::mem::size_of::<T>(),
            )
        };
        let src_bytes = UserPtr::<u8>::new(src.addr());
        match self.copy_from_user(dst_bytes, src_bytes, guard) {
            V3::Done(_) | V3::Continue { .. } => {
                // SAFETY: copy_from_user wrote `size_of::<T>()` bytes
                // before returning Done.
                V3::Done(unsafe { value.assume_init() })
            }
            V3::Yield { shape, .. } => V3::Yield {
                progress: NoProgress,
                shape,
            },
            V3::Err(e) => V3::Err(e),
        }
    }

    /// Write a `T: Copy` value to user-space `dst`. Wrapper over
    /// `copy_to_user`.
    pub fn write_user<T: Copy>(
        &self,
        dst: UserPtr<T>,
        value: T,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        use step_engine::{NoProgress, StepOutcome as V3};
        // SAFETY: addr_of! yields a valid kernel pointer to `value`
        // for the lifetime of this call; the cast to *const u8 reads
        // exactly `size_of::<T>()` bytes from a Copy value.
        let src_bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::addr_of!(value) as *const u8,
                core::mem::size_of::<T>(),
            )
        };
        let dst_bytes = UserPtr::<u8>::new(dst.addr());
        match self.copy_to_user(dst_bytes, src_bytes, guard) {
            V3::Done(_) | V3::Continue { .. } => V3::Done(()),
            V3::Yield { shape, .. } => V3::Yield {
                progress: NoProgress,
                shape,
            },
            V3::Err(e) => V3::Err(e),
        }
    }

    /// Eagerly fault-in every page in `range` for access of `kind` and
    /// publish each page through the pmap. After a successful `Done(())`
    /// every page in `range` has a published pmap entry whose
    /// `Prot.permits(kind.required_prot())` holds; subsequent
    /// `copy_*_user` calls find the page through `pmap.lookup` and do
    /// not re-materialise.
    ///
    /// Implementation mirrors `vm::execution::fault_script`'s tail
    /// synchronously: for each page, observe the recipe through
    /// `resolve_fault`, materialise via `materialize_pagebacked`, and
    /// publish through `pmap.publish_page_with_replacement`. Already-
    /// published pages whose protection already permits the access
    /// are skipped, so calling this twice on the same range is cheap.
    ///
    /// Errors and Blocked propagate to the caller:
    ///
    /// - `Err(Errno::EFAULT)` for unmapped pages (no recipe), for
    ///   prot-mismatch (recipe rejects the access), and for
    ///   materialisation / publication failures from the underlying
    ///   subsystems (`VmFault` / pmap surface them as opaque internal
    ///   shapes; the user-VA contract collapses every one to EFAULT).
    /// - `Blocked(token)` if a page-cache fetch needs to await; the
    ///   caller awaits the wait source and retries.
    ///
    /// Per VM_v1_2 §"No rmap": private anon frames are tracked only
    /// via published PTEs, so the pmap is the canonical authoritative
    /// store consulted by `copy_*_user`. Reserving up-front guarantees
    /// every page lands once and stays observable.
    ///
    /// Does **not** take a `&Guard<'_>` parameter — internal
    /// observation paths (`resolve_fault` → `require_fault_recipe` →
    /// `aspace.lookup`) take their own fresh epoch guards. Callers
    /// holding an outer guard must drop it before invoking this method
    /// to avoid `epoch guards cannot be nested on the same CPU`.
    pub fn reserve_user_range_for_access(
        &self,
        range: UserRange,
        kind: UserAccessKind,
    ) -> StepOutcome<(), NoProgress> {
        use crate::page_backed::adapter::step_engine::StepOutcome as V3;
        for page in range.iter_pages() {
            let page_addr = match page.checked_start_addr() {
                Ok(addr) => addr,
                Err(_) => return V3::err(Errno::EFAULT.into()),
            };
            let page_range = match UserRange::containing_page(page_addr) {
                Ok(range) => range,
                Err(_) => return V3::err(Errno::EFAULT.into()),
            };
            // Keep the same per-page publication discipline as the trap fault
            // path. This covers the pmap fast-path check, recipe observation,
            // private-page materialization and final PTE install.
            let _page_guard = match self
                .range_lock
                .acquire_step(page_range, LockMode::Materializer)
            {
                V3::Done(guard) => guard,
                V3::Yield { shape, .. } => {
                    return V3::Yield {
                        progress: NoProgress,
                        shape,
                    };
                }
                _ => return V3::err(Errno::EFAULT.into()),
            };
            // Skip pages already published with sufficient protection.
            // We only avoid re-materialisation when the cached entry
            // already permits the requested access.
            if let Some(snapshot) = self.pmap.lookup(page) {
                if snapshot.prot.permits(kind.required_prot()) {
                    continue;
                }
                // Insufficient cached protection is not a hard fault:
                // fork CoW deliberately leaves parent private pages
                // mapped read-only. If the recipe permits the requested
                // access, fall through and materialise/publish the
                // writable private page below.
            }
            // Build a synthetic fault, observe the recipe, materialise,
            // and publish synchronously.
            let fault = VmFault::new(page_addr, kind.required_prot());
            let outcome: VmFaultOutcome = match require_fault_recipe(self, fault) {
                Ok(o) => o,
                Err(e) => {
                    // PROBE(git fork-exec EFAULT hunt): which VA, which error.
                    crate::vm::probe::probe_emit(
                        "resv-fault",
                        &[page_addr.0 as u64, vm_fault_error_probe_code(&e)],
                    );
                    return V3::err(Errno::EFAULT.into());
                }
            };
            let materialization = match outcome.materialize_pagebacked() {
                Ok(m) => m,
                Err(_) => {
                    crate::vm::probe::probe_emit("resv-mat", &[page_addr.0 as u64]);
                    return V3::err(Errno::EFAULT.into());
                }
            };
            // Publish the materialisation. `replace_existing` honours
            // the materialisation's own intent (private CoW path sets
            // it; otherwise false). The page-key derives from the
            // outcome's page_range, mirroring `fault_script`.
            if self
                .pmap
                .publish_page_with_replacement(
                    outcome.page_range.start().containing_page(),
                    materialization.page.ppn,
                    materialization.publish_prot,
                    materialization.page.map_pin,
                    materialization.replace_existing,
                )
                .is_err()
            {
                crate::vm::probe::probe_emit("resv-pub", &[page_addr.0 as u64]);
                return V3::err(Errno::EFAULT.into());
            }
        }
        V3::done(())
    }

    /// Wait-capable counterpart of [`Self::reserve_user_range_for_access`].
    ///
    /// A concurrent first-touch, CoW publication, or binding mutation can
    /// legitimately own the page-sized `RangeLock` transaction.  That is
    /// kernel-internal scheduling state, not an I/O failure visible to the
    /// syscall caller.  Drop all per-attempt state, await the lock's release
    /// notification, and retry the complete range.  Rewalking pages already
    /// published by a previous attempt is cheap because the pmap fast path
    /// skips them.
    ///
    /// No epoch guard or `RangeGuard` is held across `.await`.
    pub async fn reserve_user_range_for_access_wait(
        &self,
        range: UserRange,
        kind: UserAccessKind,
    ) -> Result<(), Errno> {
        use crate::page_backed::adapter::step_engine::StepOutcome as V3;

        loop {
            match self.reserve_user_range_for_access(range, kind) {
                V3::Done(()) => return Ok(()),
                V3::Err(error) => return Err(error.into()),
                V3::Continue { .. } => {
                    // The current synchronous implementation never emits
                    // Continue, but retrying preserves the step contract if a
                    // future backend starts using it.
                    continue;
                }
                V3::Yield { shape, .. } => {
                    let token =
                        crate::vm::notification::wait_token_from_shape(&shape).ok_or(Errno::EIO)?;
                    if let Some(wait) = crate::wait_source::wait_on_registered_source_id(
                        token.source_id(),
                        token.interest(),
                    ) {
                        let _ = wait.await;
                    }
                    // `None` means the release raced ahead of registration.
                    // Retrying immediately is both safe and necessary.
                }
            }
        }
    }

    /// Read a NUL-terminated byte string starting at `src`, capped at
    /// `max_len` bytes. Returns the bytes (NUL terminator stripped).
    ///
    /// `src.addr() == 0` returns `Done(empty)` — matches the
    /// bake-in-fixture-friendly behaviour of the old shim helpers
    /// (`tx-shims::read_user_cstr`). Callers that need a stricter
    /// "NULL is EFAULT" rule should reject the NULL pointer before
    /// invoking.
    ///
    /// Returns `Errno::ENAMETOOLONG` if `max_len` bytes are walked
    /// without finding a NUL, or `Errno::ENOMEM` if the kernel-owned
    /// result buffer cannot grow.
    pub fn read_user_cstr(
        &self,
        src: UserPtr<u8>,
        max_len: usize,
        guard: &Guard<'_>,
    ) -> StepOutcome<Vec<u8>, NoProgress> {
        use step_engine::{NoProgress, StepOutcome as V3};
        if src.addr() == 0 || max_len == 0 {
            return V3::Done(Vec::new());
        }
        let mut out: Vec<u8> = Vec::new();
        if out.try_reserve_exact(core::cmp::min(max_len, 256)).is_err() {
            return V3::Err(Errno::ENOMEM.into());
        }
        let mut consumed = 0usize;
        while consumed < max_len {
            let Some(user_addr) = src.addr().checked_add(consumed) else {
                return V3::Err(Errno::EFAULT.into());
            };
            let page_addr = user_addr & !(USER_PAGE_SIZE - 1);
            let within = user_addr - page_addr;
            let chunk = core::cmp::min(max_len - consumed, USER_PAGE_SIZE - within);
            let frame_base =
                match resolve_user_page_addr(self, page_addr, UserAccessKind::Read, guard) {
                    ResolveOutcome::Done(addr) => addr,
                    ResolveOutcome::Err(e) => return V3::Err(e.into()),
                    ResolveOutcome::Blocked(t) => {
                        return crate::vm::notification::yield_wait_token(NoProgress, t);
                    }
                };
            // SAFETY: frame_base.add(within) is a valid kernel
            // direct-map pointer to the requested user byte; we read
            // up to `chunk` bytes which fit inside `USER_PAGE_SIZE -
            // within`. The page stayed live across this scan because
            // we hold the materialisation pin via the resolve helper.
            // Note: ResolveOutcome only carries the bare address — we
            // re-resolve every page rather than threading the
            // `MaterializedPage` through the loop because the
            // c-string scan may exit mid-page.
            for i in 0..chunk {
                let byte = unsafe { core::ptr::read(frame_base.add(within + i)) };
                if byte == 0 {
                    return V3::Done(out);
                }
                if out.len() == out.capacity() {
                    let additional =
                        core::cmp::min(out.capacity().max(1), max_len.saturating_sub(out.len()));
                    if out.try_reserve(additional).is_err() {
                        return V3::Err(Errno::ENOMEM.into());
                    }
                }
                out.push(byte);
            }
            consumed += chunk;
        }
        V3::Err(Errno::ENAMETOOLONG.into())
    }
}

fn copy_in(
    aspace: &AddressSpace,
    dst: &mut [u8],
    src: UserPtr<u8>,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    use step_engine::{ByteProgress, StepOutcome as V3};
    emit_vm_user_trace(b"debug.vm.user.copy_in.len", dst.len() as i64);
    if dst.is_empty() {
        emit_vm_user_trace(b"debug.vm.user.copy_in.phase", 9);
        return V3::Done(0);
    }
    if src.addr() == 0 {
        emit_vm_user_trace(b"debug.vm.user.copy_in.err", 1);
        return V3::Err(Errno::EFAULT.into());
    }
    let mut copied = 0usize;
    let total = dst.len();
    while copied < total {
        let Some(user_addr) = src.addr().checked_add(copied) else {
            return if copied > 0 {
                V3::Done(copied)
            } else {
                V3::Err(Errno::EFAULT.into())
            };
        };
        let page_addr = user_addr & !(USER_PAGE_SIZE - 1);
        let within = user_addr - page_addr;
        let chunk = core::cmp::min(total - copied, USER_PAGE_SIZE - within);
        emit_vm_user_trace(b"debug.vm.user.copy_in.chunk", chunk as i64);
        emit_vm_user_trace(b"debug.vm.user.copy_in.phase", 0);
        match resolve_user_page_addr(aspace, page_addr, UserAccessKind::Read, guard) {
            ResolveOutcome::Done(frame_base) => {
                emit_vm_user_trace(b"debug.vm.user.copy_in.phase", 1);
                // SAFETY: `frame_base.add(within)` is a valid kernel
                // direct-map pointer to the requested user byte; we
                // copy exactly `chunk <= USER_PAGE_SIZE - within`
                // bytes. The destination slice has at least `chunk`
                // bytes remaining at offset `copied`. The
                // materialisation pin keeps the frame live until
                // `resolve_user_page_addr` returns; the copy
                // completes synchronously here.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        frame_base.add(within),
                        dst.as_mut_ptr().add(copied),
                        chunk,
                    );
                }
                emit_vm_user_trace(b"debug.vm.user.copy_in.phase", 2);
                copied += chunk;
                emit_vm_user_trace(b"debug.vm.user.copy_in.copied", copied as i64);
            }
            ResolveOutcome::Err(e) => {
                emit_vm_user_trace(b"debug.vm.user.copy_in.err", 2);
                if copied > 0 {
                    return V3::Done(copied);
                }
                return V3::Err(e.into());
            }
            ResolveOutcome::Blocked(t) => {
                emit_vm_user_trace(b"debug.vm.user.copy_in.blocked", 1);
                return crate::vm::notification::yield_wait_token(ByteProgress::new(copied), t);
            }
        }
    }
    emit_vm_user_trace(b"debug.vm.user.copy_in.phase", 3);
    V3::Done(copied)
}

fn copy_out(
    aspace: &AddressSpace,
    dst: UserPtr<u8>,
    src: &[u8],
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    use step_engine::{ByteProgress, StepOutcome as V3};
    if src.is_empty() {
        return V3::Done(0);
    }
    if dst.addr() == 0 {
        return V3::Err(Errno::EFAULT.into());
    }
    let mut copied = 0usize;
    let total = src.len();
    while copied < total {
        let Some(user_addr) = dst.addr().checked_add(copied) else {
            return if copied > 0 {
                V3::Done(copied)
            } else {
                V3::Err(Errno::EFAULT.into())
            };
        };
        let page_addr = user_addr & !(USER_PAGE_SIZE - 1);
        let within = user_addr - page_addr;
        let chunk = core::cmp::min(total - copied, USER_PAGE_SIZE - within);
        match resolve_user_page_addr(aspace, page_addr, UserAccessKind::Write, guard) {
            ResolveOutcome::Done(frame_base) => {
                // SAFETY: same as `copy_in`, with direction reversed
                // — frame_base is the kernel direct-map view of the
                // user-page being written; the source slice has at
                // least `chunk` bytes remaining at offset `copied`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src.as_ptr().add(copied),
                        frame_base.add(within),
                        chunk,
                    );
                }
                copied += chunk;
            }
            ResolveOutcome::Err(e) => {
                if copied > 0 {
                    return V3::Done(copied);
                }
                return V3::Err(e.into());
            }
            ResolveOutcome::Blocked(t) => {
                return crate::vm::notification::yield_wait_token(ByteProgress::new(copied), t);
            }
        }
    }
    V3::Done(copied)
}

/// Outcome of resolving one user page to its kernel direct-map
/// address.
enum ResolveOutcome {
    /// Kernel direct-map base pointer for the resolved page. The
    /// `MaterializedPage` pin is dropped before returning, so the
    /// pointer is only safe for the synchronous remainder of the
    /// containing tool-step. Both `copy_in` / `copy_out` use the
    /// pointer immediately for one `copy_nonoverlapping` and discard
    /// it; `read_user_cstr` re-resolves per page.
    Done(*mut u8),
    Err(Errno),
    Blocked(WaitToken),
}

/// Look up the `VmEntry` covering `page_addr`, validate the access
/// against its `Prot`, materialise the page through its backing, and
/// translate the resulting `Ppn` into a kernel direct-map address.
///
/// Pmap-first probe (per VM_v1_2 §"No rmap"): if the page already has
/// a published pmap entry whose `Prot` permits the access, return the
/// cached frame's kernel direct-map address directly. The frame is
/// kept resident by the existing pmap entry's `MapPin`; the caller
/// only borrows the pointer for the synchronous remainder of the
/// containing tool-step (matching the existing `ResolveOutcome::Done`
/// contract — the pin is **not** plumbed through). On miss (or
/// insufficient cached prot), fall through to the per-call materialise
/// path which preserves the original eager-walk semantics.
///
/// Without the pmap-first probe, `VmBacking::PrivateAnon` would
/// allocate a fresh zero frame on every call, breaking cross-call
/// consistency: a `copy_to_user` write would not be visible to a
/// subsequent `copy_from_user` read on the same page. brk-backed
/// userspace heap (musl malloc / argv strings) requires this
/// consistency.
fn resolve_user_page_addr(
    aspace: &AddressSpace,
    page_addr: usize,
    kind: UserAccessKind,
    guard: &Guard<'_>,
) -> ResolveOutcome {
    let user_page = UserVirtAddr(page_addr).containing_page();
    let kind_id = match kind {
        UserAccessKind::Read => 0,
        UserAccessKind::Write => 1,
    };
    emit_vm_user_trace(b"debug.vm.user.resolve.kind", kind_id);
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 0);

    // Pmap-first lookup. The published mapping pins the frame; we
    // borrow the kernel direct-map pointer for the synchronous copy
    // and release it before returning, matching `ResolveOutcome::Done`'s
    // existing contract (no MapPin threaded out). This is the hot path
    // for repeated user copies from pthread stack/TLS pages that are
    // already resident.
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 1);
    let cached = aspace.pmap.lookup(user_page);
    if let Some(snapshot) = cached {
        if snapshot.prot.permits(kind.required_prot()) {
            emit_vm_user_trace(b"debug.vm.user.resolve.phase", 2);
            return match page_allocator::frame_kernel_addr(snapshot.ppn) {
                Ok(p) => {
                    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 3);
                    ResolveOutcome::Done(p)
                }
                Err(_) => {
                    emit_vm_user_trace(b"debug.vm.user.resolve.err", 1);
                    ResolveOutcome::Err(Errno::EFAULT)
                }
            };
        }
        emit_vm_user_trace(b"debug.vm.user.resolve.phase", 4);
        // Cached mapping rejects this access. The recipe may still
        // permit it (for example, a read-only PTE published for a
        // private-anon first-touch read followed by a write), so fall
        // through to recipe validation and materialise + publish.
    } else {
        emit_vm_user_trace(b"debug.vm.user.resolve.phase", 5);
    }

    // Use the in-vm `recipes.lookup(addr, guard)` directly rather
    // than `aspace.lookup(addr)` so the caller's existing epoch
    // guard is reused. Allocating a nested `epoch::guard()` here
    // panics on the host test harness.
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 6);
    let entry = match aspace.recipes.lookup(UserVirtAddr(page_addr), guard) {
        Some(e) => e,
        None => {
            emit_vm_user_trace(b"debug.vm.user.resolve.err", 2);
            return ResolveOutcome::Err(Errno::EFAULT);
        }
    };
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 7);
    emit_vm_user_trace(
        b"debug.vm.user.resolve.backing",
        vm_backing_trace_id(entry.backing_kind()),
    );
    if !entry.prot.permits(kind.required_prot()) {
        emit_vm_user_trace(b"debug.vm.user.resolve.err", 3);
        return ResolveOutcome::Err(Errno::EFAULT);
    }

    // A fork can inherit an exact resident private frame through the pmap even
    // when the per-entry private set has no row for it. Before a write fault
    // falls back to the recipe backing, seed that resident frame as SharedCow.
    // The normal private materializer will then copy from the inherited bytes
    // instead of re-deriving a stale file-cache or zero page.
    if kind == UserAccessKind::Write && !entry.flags.shared {
        if let (Some(snapshot), Some(set), Some(page_off)) = (
            cached,
            entry.private(),
            entry.page_offset_of(UserVirtAddr(page_addr)),
        ) {
            if set.lookup(page_off).is_none() {
                let cache_pin = match page_allocator::acquire_cache_pin(snapshot.ppn) {
                    Ok(pin) => pin,
                    Err(_) => return ResolveOutcome::Err(Errno::EFAULT),
                };
                let frame =
                    PrivateFrame::new(snapshot.ppn, PrivateFrameState::SharedCow, cache_pin);
                match set.install_if_absent(page_off, frame) {
                    Ok(_) | Err(PrivatePageError::Conflict { .. }) => {}
                    Err(_) => return ResolveOutcome::Err(Errno::EFAULT),
                }
            }
        }
    }

    // Pmap miss (or insufficient cached prot). Materialise via the
    // backing path. We then synchronously publish the result to the
    // pmap so a subsequent `copy_*_user` over the same page lands the
    // pmap-first probe above and observes the same frame.
    //
    // Without the publish, `VmBacking::PrivateAnon` would re-zero on
    // every call (each materialisation allocates a fresh frame),
    // breaking write-then-read consistency of brk-backed heap pages.
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 8);
    let materialised = match resolve_user_page(&entry, page_addr, kind, guard) {
        ResolvePageOutcome::Done(m) => {
            emit_vm_user_trace(b"debug.vm.user.resolve.phase", 9);
            m
        }
        ResolvePageOutcome::Err(e) => {
            emit_vm_user_trace(b"debug.vm.user.resolve.err", 4);
            return ResolveOutcome::Err(e);
        }
        ResolvePageOutcome::Blocked(t) => {
            emit_vm_user_trace(b"debug.vm.user.resolve.blocked", 1);
            return ResolveOutcome::Blocked(t);
        }
    };
    // Publish via pmap so the next call hits the cache. The
    // `publish_prot` mirrors `fault_script`: read access on a
    // private-anon page publishes a read-only mapping (the next write
    // would refault and republish writable); write access publishes
    // the entry's full prot. Failure to publish is non-fatal — we
    // still have the materialised frame in hand for *this* call. The
    // next call will re-materialise (correct but slower).
    let publish_prot = match (entry.backing_kind(), kind) {
        (VmEntryBacking::PrivateAnon, UserAccessKind::Read) => entry.prot.without_write(),
        (VmEntryBacking::Page { .. }, UserAccessKind::Read) if !entry.flags.shared => {
            entry.prot.without_write()
        }
        _ => entry.prot,
    };
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 10);
    if aspace
        .pmap
        .publish_page_with_replacement(
            user_page,
            materialised.ppn,
            publish_prot,
            materialised.map_pin,
            true,
        )
        .is_err()
    {
        emit_vm_user_trace(b"debug.vm.user.resolve.err", 5);
    }
    emit_vm_user_trace(b"debug.vm.user.resolve.phase", 11);
    match page_allocator::frame_kernel_addr(materialised.ppn) {
        Ok(p) => {
            emit_vm_user_trace(b"debug.vm.user.resolve.phase", 12);
            ResolveOutcome::Done(p)
        }
        Err(_) => {
            emit_vm_user_trace(b"debug.vm.user.resolve.err", 6);
            ResolveOutcome::Err(Errno::EFAULT)
        }
    }
}

fn vm_backing_trace_id(backing: VmEntryBacking) -> i64 {
    match backing {
        VmEntryBacking::None => 0,
        VmEntryBacking::PrivateAnon => 1,
        VmEntryBacking::Page { .. } => 2,
    }
}

fn emit_vm_user_trace(name: &[u8], value: i64) {
    if !cfg!(tx_vm_user_access_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

enum ResolvePageOutcome {
    Done(MaterializedPage),
    Err(Errno),
    Blocked(WaitToken),
}

fn resolve_user_page(
    entry: &VmEntry,
    page_addr: usize,
    kind: UserAccessKind,
    guard: &Guard<'_>,
) -> ResolvePageOutcome {
    emit_vm_user_trace(
        b"debug.vm.user.resolve_page.backing",
        vm_backing_trace_id(entry.backing_kind()),
    );
    if entry.special_backing().is_some() {
        return ResolvePageOutcome::Err(Errno::EFAULT);
    }
    match entry.backing_kind() {
        VmEntryBacking::None => {
            emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 1);
            ResolvePageOutcome::Err(Errno::EFAULT)
        }
        VmEntryBacking::PrivateAnon => {
            emit_vm_user_trace(b"debug.vm.user.resolve_page.phase", 0);
            // Reuse the VM fault path's private-anon materialisation
            // for consistency: a fresh zeroed frame for read access,
            // a private writable frame for write access. Today this
            // does not share state with the per-thread fault path's
            // pmap publication — eager-walk reads of an unwritten
            // anon page may yield a different (but equally zeroed)
            // frame than a subsequent fault would publish, which
            // matches Linux's "unwritten anon = zero" semantics.
            let page_range = match crate::vm::structure::UserRange::new_aligned(
                UserVirtAddr(page_addr),
                USER_PAGE_SIZE,
            ) {
                Ok(r) => r,
                Err(_) => {
                    emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 2);
                    return ResolvePageOutcome::Err(Errno::EFAULT);
                }
            };
            let outcome = crate::vm::structure::VmFaultOutcome {
                page_range,
                private_identity: entry.private_identity(),
                entry: entry.clone(),
                access: kind.required_prot(),
                pmap_materialization_deferred: true,
            };
            emit_vm_user_trace(b"debug.vm.user.resolve_page.phase", 1);
            match outcome.materialize_pagebacked() {
                Ok(materialization) => {
                    emit_vm_user_trace(b"debug.vm.user.resolve_page.phase", 2);
                    ResolvePageOutcome::Done(materialization.page)
                }
                Err(_) => {
                    emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 3);
                    ResolvePageOutcome::Err(Errno::EFAULT)
                }
            }
        }
        VmEntryBacking::Page { offset } => {
            emit_vm_user_trace(b"debug.vm.user.resolve_page.phase", 3);
            let Some((pc, _)) = entry.page_backing() else {
                emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 4);
                return ResolvePageOutcome::Err(Errno::EFAULT);
            };
            let entry_start = entry.range.start().as_usize();
            let delta = match page_addr.checked_sub(entry_start) {
                Some(v) => v,
                None => {
                    emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 4);
                    return ResolvePageOutcome::Err(Errno::EFAULT);
                }
            };
            let backing_offset = match (delta as u64).checked_add(offset) {
                Some(v) => v,
                None => {
                    emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 5);
                    return ResolvePageOutcome::Err(Errno::EFAULT);
                }
            };
            if !backing_offset.is_multiple_of(USER_PAGE_SIZE as u64) {
                emit_vm_user_trace(b"debug.vm.user.resolve_page.err", 6);
                return ResolvePageOutcome::Err(Errno::EFAULT);
            }
            let page_index = PageIndex::new(backing_offset / USER_PAGE_SIZE as u64);
            emit_vm_user_trace(b"debug.vm.user.resolve_page.phase", 4);
            emit_vm_user_trace(b"debug.vm.user.pagebacked.phase", 0);
            // `materialize_page` is now v3
            // (`StepOutcome<MaterializedPage, NoProgress>`); translate
            // per outcome variant onto the v4 `ResolvePageOutcome`:
            // - v3 `Done(m)` → `ResolvePageOutcome::Done(m)`.
            // - v3 `Continue { .. }` (NoProgress) → `Err(EFAULT)` —
            //   page allocation rarely emits this; treating it as a
            //   fault keeps the user-access path conservative.
            // - v3 `Yield { OnWaitSource { c, i } }` →
            //   `ResolvePageOutcome::Blocked(WaitToken(c, i))`.
            // - v3 `Yield { OnAgent .. }` → `Err(EFAULT)`.
            // - v3 `Err(_)` → `Err(EFAULT)`.
            use step_engine::StepOutcome as V3;
            match pc.materialize_page(page_index, kind.materialize_access(), guard) {
                V3::Done(m) => {
                    emit_vm_user_trace(b"debug.vm.user.pagebacked.phase", 1);
                    ResolvePageOutcome::Done(m)
                }
                V3::Continue { .. } => {
                    emit_vm_user_trace(b"debug.vm.user.pagebacked.err", 1);
                    ResolvePageOutcome::Err(Errno::EFAULT)
                }
                V3::Yield { shape, .. } => {
                    if let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) {
                        emit_vm_user_trace(b"debug.vm.user.pagebacked.blocked", 1);
                        ResolvePageOutcome::Blocked(token)
                    } else {
                        emit_vm_user_trace(b"debug.vm.user.pagebacked.err", 2);
                        ResolvePageOutcome::Err(Errno::EFAULT)
                    }
                }
                V3::Err(_) => {
                    emit_vm_user_trace(b"debug.vm.user.pagebacked.err", 3);
                    ResolvePageOutcome::Err(Errno::EFAULT)
                }
            }
        }
    }
}


/// PROBE(git fork-exec EFAULT hunt): stable small codes for
/// `VmFaultError` variants so the vmwatch line can carry the cause.
fn vm_fault_error_probe_code(e: &crate::vm::VmFaultError) -> u64 {
    use crate::vm::VmFaultError as E;
    match e {
        E::Range(_) => 1,
        E::NoRecipe => 2,
        E::ProtectionViolation => 3,
        E::WouldBlock => 4,
        E::BackingMismatch => 5,
        E::BackingOffsetOverflow => 6,
        E::PageBeyondSize => 7,
        E::PageCache(_) => 8,
        E::SpecialUnavailable => 9,
        E::StaleRecipe => 10,
        E::Pmap(_) => 11,
    }
}
