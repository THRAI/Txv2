//! Single-binding publication over an existing Zone slot.
//!
//! Readers observe the root under a caller-owned epoch guard. Writers retain
//! the current target through private strong evidence and publish only after
//! that evidence is installed.

use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::epoch::Guard;
use crate::SpinMutex;

use super::meta::SlotState;
use super::policy::IsPayloadPolicy;
use super::registry;
use super::slot::Slot;
use super::{BindingToken, Cap, Dead, IdentRef, PayloadCap, ZoneAllocated};

#[cfg(any(test, tx_cap_upgrade_metrics))]
const PUBLISHED_BINDING_RETAIN_TRACE_NAME: &[u8] =
    b"debug.cap.upgrade.published_binding.retain.attempts";

#[cfg(test)]
static RETAIN_ATTEMPTS_FOR_TEST: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[inline(always)]
fn emit_retain_attempt() {
    #[cfg(test)]
    RETAIN_ATTEMPTS_FOR_TEST.fetch_add(1, Ordering::Relaxed);
    #[cfg(tx_cap_upgrade_metrics)]
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(PUBLISHED_BINDING_RETAIN_TRACE_NAME, 1);
    }
}

/// Lock-free visibility for one current Zone-backed binding.
///
/// Only `Cap<T>` and payload-policy `PayloadCap<T>` have an operation family:
///
/// ```
/// use tx_substrate::zone::{Cap, PublishedBinding, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Binding = PublishedBinding<Object, Cap<Object>>;
/// let _ = Binding::empty();
/// ```
///
/// ```compile_fail
/// use tx_substrate::epoch::Guard;
/// use tx_substrate::zone::{PublishedBinding, Weak, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Bad = PublishedBinding<Object, Weak<Object>>;
/// fn forbidden(binding: &Bad, next: Weak<Object>, guard: &Guard<'_>) {
///     let _ = Bad::installed(next);
///     let _ = binding.replace(next);
///     let _ = binding.retain(guard);
///     let _ = binding.observe(guard);
/// }
/// ```
///
/// ```compile_fail
/// use tx_substrate::epoch::Guard;
/// use tx_substrate::zone::{PublishedBinding, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Bad = PublishedBinding<Object, u32>;
/// fn forbidden(binding: &Bad, next: u32, guard: &Guard<'_>) {
///     let _ = Bad::installed(next);
///     let _ = binding.replace(next);
///     let _ = binding.retain(guard);
///     let _ = binding.observe(guard);
/// }
/// ```
///
/// A guarded observation cannot outlive the Guard that authorized it:
///
/// ```compile_fail
/// use tx_substrate::epoch;
/// use tx_substrate::zone::{Cap, IdentRef, PublishedBinding, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Binding = PublishedBinding<Object, Cap<Object>>;
/// fn escape<'a>(binding: &'a Binding) -> IdentRef<'a, Object> {
///     let guard = epoch::guard();
///     binding.observe(&guard).unwrap()
/// }
/// ```
///
/// Guarded observations are CPU-local and cannot move to another thread:
///
/// ```compile_fail
/// use tx_substrate::epoch;
/// use tx_substrate::zone::{Cap, PublishedBinding, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Binding = PublishedBinding<Object, Cap<Object>>;
/// fn send(binding: &Binding) {
///     let guard = epoch::guard();
///     let observed = binding.observe(&guard).unwrap();
///     std::thread::scope(|scope| {
///         scope.spawn(move || drop(observed));
///     });
/// }
/// ```
///
/// A scheduler-owned future cannot carry a Guard or `IdentRef` across await:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use tx_substrate::epoch;
/// use tx_substrate::zone::{Cap, PublishedBinding, Zone, ZoneAllocated};
/// struct Object;
/// static OBJECT_ZONE: Zone<Object> = Zone::const_new();
/// unsafe impl ZoneAllocated for Object {
///     fn zone() -> &'static Zone<Self> { &OBJECT_ZONE }
/// }
/// type Binding = PublishedBinding<Object, Cap<Object>>;
/// async fn suspend(binding: Arc<Binding>) {
///     let guard = epoch::guard();
///     let observed = binding.observe(&guard).unwrap();
///     core::future::pending::<()>().await;
///     drop(observed);
/// }
/// fn require_schedulable<F: core::future::Future + Send + 'static>(future: F) {
///     drop(future);
/// }
/// fn forbidden(binding: Arc<Binding>) {
///     require_schedulable(suspend(binding));
/// }
/// ```
#[repr(C)]
pub struct PublishedBinding<T: ZoneAllocated, E> {
    root: AtomicPtr<Slot<T>>,
    writer: SpinMutex<Option<E>>,
    _marker: PhantomData<fn() -> T>,
}

trait BindingEvidence<T: ZoneAllocated>: Clone {
    fn binding_token(&self) -> BindingToken<T>;
    fn from_cap(cap: Cap<T>) -> Self;
}

impl<T: ZoneAllocated> BindingEvidence<T> for Cap<T> {
    fn binding_token(&self) -> BindingToken<T> {
        Cap::binding_token(self)
    }

    fn from_cap(cap: Cap<T>) -> Self {
        cap
    }
}

impl<T: ZoneAllocated> BindingEvidence<T> for PayloadCap<T>
where
    T::Policy: IsPayloadPolicy,
{
    fn binding_token(&self) -> BindingToken<T> {
        PayloadCap::binding_token(self)
    }

    fn from_cap(cap: Cap<T>) -> Self {
        PayloadCap::from_cap(cap)
    }
}

// The bound is deliberately private: it seals the shared implementation while
// the public method families below remain concrete Cap/PayloadCap impls.
#[allow(private_bounds)]
impl<T, E> PublishedBinding<T, E>
where
    T: ZoneAllocated,
    E: BindingEvidence<T>,
{
    const fn empty_inner() -> Self {
        Self {
            root: AtomicPtr::new(ptr::null_mut()),
            writer: SpinMutex::new(None),
            _marker: PhantomData,
        }
    }

    fn installed_inner(evidence: E) -> Self {
        let slot = live_slot_for_evidence(&evidence);
        Self {
            root: AtomicPtr::new(slot.as_ptr()),
            writer: SpinMutex::new(Some(evidence)),
            _marker: PhantomData,
        }
    }

    fn observe_inner<'g>(&self, guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        self.observe_inner_loop(guard, |_| {})
    }

    fn retain_inner(&self, guard: &Guard<'_>) -> Option<E> {
        self.retain_inner_loop(guard, |_| {})
    }

    fn retain_inner_loop<F>(&self, guard: &Guard<'_>, mut after_observe: F) -> Option<E>
    where
        F: FnMut(usize),
    {
        let first = self.observe_inner(guard)?;
        after_observe(0);
        if let Ok(cap) = first.to_cap() {
            return Some(E::from_cap(cap));
        }

        let Some(second) = self.observe_inner(guard) else {
            return None;
        };
        after_observe(1);
        match second.to_cap() {
            Ok(cap) => Some(E::from_cap(cap)),
            Err(Dead) => self.writer.lock().clone(),
        }
    }

    #[inline(always)]
    fn observe_inner_loop<'g, F>(
        &self,
        guard: &'g Guard<'_>,
        mut after_root_load: F,
    ) -> Option<IdentRef<'g, T>>
    where
        F: FnMut(*mut Slot<T>),
    {
        loop {
            let first = self.root.load(Ordering::Acquire);
            after_root_load(first);
            let slot = NonNull::new(first)?;
            let word = unsafe { slot.as_ref().meta().load(Ordering::Acquire) };
            if word.state() == SlotState::Live {
                return Some(unsafe {
                    IdentRef::from_published_slot(slot, word.generation(), guard)
                });
            }

            if self.root.load(Ordering::Acquire) != first {
                continue;
            }
            panic!("PublishedBinding points at a non-Live Zone slot");
        }
    }

    #[cfg(test)]
    fn observe_inner_with_hook<'g, F>(
        &self,
        guard: &'g Guard<'_>,
        after_root_load: F,
    ) -> Option<IdentRef<'g, T>>
    where
        F: FnMut(*mut Slot<T>),
    {
        self.observe_inner_loop(guard, after_root_load)
    }

    fn replace_inner(&self, next: E) -> Option<E> {
        let next_ptr = live_slot_for_evidence(&next).as_ptr();
        let mut writer = self.writer.lock();
        let old = writer.replace(next);
        let expected_old_ptr = evidence_option_ptr(old.as_ref());
        let swapped = self.root.swap(next_ptr, Ordering::AcqRel);
        assert_eq!(
            swapped, expected_old_ptr,
            "PublishedBinding root/evidence mismatch during replace"
        );
        drop(writer);
        old
    }

    fn withdraw_inner(&self) -> Option<E> {
        let mut writer = self.writer.lock();
        let swapped = self.root.swap(ptr::null_mut(), Ordering::AcqRel);
        let old = writer.take();
        assert_eq!(
            swapped,
            evidence_option_ptr(old.as_ref()),
            "PublishedBinding root/evidence mismatch during withdraw"
        );
        drop(writer);
        old
    }

    fn clear_if_token_inner(&self, expected: BindingToken<T>) -> Option<E> {
        let mut writer = self.writer.lock();
        if writer.as_ref().map(BindingEvidence::binding_token) != Some(expected) {
            assert_eq!(
                self.root.load(Ordering::Acquire),
                evidence_option_ptr(writer.as_ref()),
                "PublishedBinding root/evidence mismatch during conditional clear"
            );
            return None;
        }

        let swapped = self.root.swap(ptr::null_mut(), Ordering::AcqRel);
        let old = writer.take();
        assert_eq!(
            swapped,
            evidence_option_ptr(old.as_ref()),
            "PublishedBinding root/evidence mismatch during conditional clear"
        );
        drop(writer);
        old
    }
}

impl<T: ZoneAllocated> PublishedBinding<T, Cap<T>> {
    pub const fn empty() -> Self {
        Self::empty_inner()
    }

    pub fn installed(evidence: Cap<T>) -> Self {
        Self::installed_inner(evidence)
    }

    pub fn observe<'g>(&self, guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        self.observe_inner(guard)
    }

    pub fn retain(&self, guard: &Guard<'_>) -> Option<Cap<T>> {
        emit_retain_attempt();
        self.retain_inner(guard)
    }

    #[cfg(test)]
    fn retain_with_hook_for_test<F>(&self, guard: &Guard<'_>, hook: F) -> Option<Cap<T>>
    where
        F: FnMut(usize),
    {
        emit_retain_attempt();
        self.retain_inner_loop(guard, hook)
    }

    pub fn replace(&self, next: Cap<T>) -> Option<Cap<T>> {
        self.replace_inner(next)
    }

    pub fn withdraw(&self) -> Option<Cap<T>> {
        self.withdraw_inner()
    }

    pub fn clear_if_token(&self, expected: BindingToken<T>) -> Option<Cap<T>> {
        self.clear_if_token_inner(expected)
    }
}

impl<T> PublishedBinding<T, PayloadCap<T>>
where
    T: ZoneAllocated,
    T::Policy: IsPayloadPolicy,
{
    pub const fn empty() -> Self {
        Self::empty_inner()
    }

    pub fn installed(evidence: PayloadCap<T>) -> Self {
        Self::installed_inner(evidence)
    }

    pub fn observe<'g>(&self, guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        self.observe_inner(guard)
    }

    pub fn retain(&self, guard: &Guard<'_>) -> Option<PayloadCap<T>> {
        emit_retain_attempt();
        self.retain_inner(guard)
    }

    #[cfg(test)]
    fn retain_with_hook_for_test<F>(&self, guard: &Guard<'_>, hook: F) -> Option<PayloadCap<T>>
    where
        F: FnMut(usize),
    {
        emit_retain_attempt();
        self.retain_inner_loop(guard, hook)
    }

    pub fn replace(&self, next: PayloadCap<T>) -> Option<PayloadCap<T>> {
        self.replace_inner(next)
    }

    pub fn withdraw(&self) -> Option<PayloadCap<T>> {
        self.withdraw_inner()
    }

    pub fn clear_if_token(&self, expected: BindingToken<T>) -> Option<PayloadCap<T>> {
        self.clear_if_token_inner(expected)
    }
}

impl<T: ZoneAllocated, E> Drop for PublishedBinding<T, E> {
    fn drop(&mut self) {
        *self.root.get_mut() = ptr::null_mut();
        let evidence = self.writer.lock().take();
        drop(evidence);
    }
}

fn live_slot_for_evidence<T, E>(evidence: &E) -> NonNull<Slot<T>>
where
    T: ZoneAllocated,
    E: BindingEvidence<T>,
{
    let token = evidence.binding_token();
    let slot = registry::slot_for::<T>(token.key())
        .expect("PublishedBinding evidence must resolve to its Zone slot");
    let word = unsafe { slot.as_ref().meta().load(Ordering::Acquire) };
    assert_eq!(
        word.state(),
        SlotState::Live,
        "PublishedBinding evidence must retain a Live Zone slot"
    );
    assert_eq!(
        word.generation(),
        token.generation(),
        "PublishedBinding evidence generation must match its Zone slot"
    );
    slot
}

fn evidence_option_ptr<T, E>(evidence: Option<&E>) -> *mut Slot<T>
where
    T: ZoneAllocated,
    E: BindingEvidence<T>,
{
    evidence.map_or(ptr::null_mut(), |value| {
        live_slot_for_evidence(value).as_ptr()
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::mem::{align_of, offset_of, size_of};
    use core::sync::atomic::Ordering;

    use super::*;
    use crate::epoch;
    use crate::zone::{self, SlotState, Zone};

    static RACE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct LayoutIdentity;

    static LAYOUT_ZONE: Zone<LayoutIdentity> = Zone::const_new();

    unsafe impl ZoneAllocated for LayoutIdentity {
        fn zone() -> &'static Zone<Self> {
            &LAYOUT_ZONE
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RaceIdentity {
        id: u32,
    }

    static RACE_ZONE: Zone<RaceIdentity> = Zone::const_new();

    unsafe impl ZoneAllocated for RaceIdentity {
        type Policy = crate::zone::PayloadPolicy<Self>;

        fn zone() -> &'static Zone<Self> {
            &RACE_ZONE
        }
    }

    fn reset_race_zone() -> std::sync::MutexGuard<'static, ()> {
        let isolation = RACE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::testing::init_host_for_test_once();
        unsafe {
            epoch::testing::reset_for_test();
            zone::testing::reset_for_test();
        }
        epoch::testing::init_for_test();
        zone::testing::init_for_test(
            4096,
            crate::page_allocator::testing::direct_map_base_for_test(),
        )
        .expect("test zone runtime init");
        zone::register_zone_for::<RaceIdentity>().expect("race zone registration");
        isolation
    }

    #[test]
    fn root_is_first_and_writer_immediately_follows() {
        type Binding = PublishedBinding<LayoutIdentity, Cap<LayoutIdentity>>;

        assert_eq!(offset_of!(Binding, root), 0);
        assert_eq!(offset_of!(Binding, writer), size_of::<usize>());
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<Binding>(), 24);
            assert_eq!(align_of::<Binding>(), 8);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(size_of::<Binding>(), 16);
            assert_eq!(align_of::<Binding>(), 4);
        }
    }

    #[test]
    fn retain_metric_name_is_stable() {
        assert_eq!(
            PUBLISHED_BINDING_RETAIN_TRACE_NAME,
            b"debug.cap.upgrade.published_binding.retain.attempts"
        );
    }

    #[test]
    fn reader_retries_when_old_root_retires_after_root_changes() {
        let _isolation = reset_race_zone();
        type Binding = PublishedBinding<RaceIdentity, Cap<RaceIdentity>>;

        let first = zone::sign(RaceIdentity { id: 1 }).expect("first identity");
        let first_key = first.key();
        let second = zone::sign(RaceIdentity { id: 2 }).expect("second identity");
        let binding = Binding::installed(first);
        let guard = epoch::guard();
        let mut replacement = Some(second);

        let observed = binding
            .observe_inner_with_hook(&guard, |_| {
                if let Some(next) = replacement.take() {
                    drop(binding.replace(next).expect("old binding evidence"));
                }
            })
            .expect("reader retries to replacement");

        assert_eq!(observed.id, 2);
        assert_eq!(
            zone::testing::slot_word::<RaceIdentity>(first_key)
                .expect("old slot")
                .state(),
            SlotState::Retiring
        );
    }

    #[test]
    fn reader_panics_when_stable_root_points_at_non_live_slot() {
        let _isolation = reset_race_zone();
        type Binding = PublishedBinding<RaceIdentity, Cap<RaceIdentity>>;

        let identity = zone::sign(RaceIdentity { id: 7 }).expect("identity");
        let binding = Binding::installed(identity);
        let guard = epoch::guard();
        let mut original = None;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = binding.observe_inner_with_hook(&guard, |root: *mut Slot<RaceIdentity>| {
                if original.is_some() {
                    return;
                }
                let slot = NonNull::new(root).expect("installed root");
                let meta = unsafe { slot.as_ref().meta() };
                let live = meta.load(Ordering::Acquire);
                original = Some((slot, live));
                meta.compare_exchange(
                    live,
                    live.with_state(SlotState::Retiring),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .expect("test-only Live to Retiring mutation");
            });
        }));

        let (slot, live) = original.expect("test hook fired");
        let meta = unsafe { slot.as_ref().meta() };
        let retiring = meta.load(Ordering::Acquire);
        meta.compare_exchange(retiring, live, Ordering::AcqRel, Ordering::Acquire)
            .expect("restore test-only metadata mutation");

        assert!(result.is_err());
    }

    #[test]
    fn retained_read_cap_recovers_after_forced_replace() {
        let _isolation = reset_race_zone();
        let first = zone::sign(RaceIdentity { id: 1 }).expect("first identity");
        let second = zone::sign(RaceIdentity { id: 2 }).expect("second identity");
        let binding = PublishedBinding::<RaceIdentity, Cap<RaceIdentity>>::installed(first);
        let guard = epoch::guard();
        let mut second = Some(second);

        let retained = binding.retain_with_hook_for_test(&guard, |attempt| match attempt {
            0 => drop(binding.replace(second.take().expect("single replacement"))),
            1 => {}
            other => panic!("unexpected observation index {other}"),
        });

        assert_eq!(retained.expect("replacement remains non-empty").id, 2);
    }

    #[test]
    fn retained_read_cap_uses_writer_fallback_after_second_replace() {
        let _isolation = reset_race_zone();
        let first = zone::sign(RaceIdentity { id: 1 }).expect("first identity");
        let second = zone::sign(RaceIdentity { id: 2 }).expect("second identity");
        let third = zone::sign(RaceIdentity { id: 3 }).expect("third identity");
        let binding = PublishedBinding::<RaceIdentity, Cap<RaceIdentity>>::installed(first);
        let guard = epoch::guard();
        let mut second = Some(second);
        let mut third = Some(third);

        let retained = binding.retain_with_hook_for_test(&guard, |attempt| match attempt {
            0 => drop(binding.replace(second.take().expect("first replacement"))),
            1 => drop(binding.replace(third.take().expect("second replacement"))),
            other => panic!("unexpected observation index {other}"),
        });

        assert_eq!(retained.expect("writer fallback retains current").id, 3);
    }

    #[test]
    fn retained_read_cap_returns_none_after_forced_withdraw() {
        let _isolation = reset_race_zone();
        let first = zone::sign(RaceIdentity { id: 1 }).expect("first identity");
        let binding = PublishedBinding::<RaceIdentity, Cap<RaceIdentity>>::installed(first);
        let guard = epoch::guard();
        let mut withdrew = false;

        let retained = binding.retain_with_hook_for_test(&guard, |attempt| {
            assert_eq!(attempt, 0);
            assert!(!core::mem::replace(&mut withdrew, true));
            drop(binding.withdraw());
        });

        assert!(retained.is_none());
    }

    #[test]
    fn retained_read_payload_cap_matches_cap_race_semantics() {
        let _isolation = reset_race_zone();
        let first =
            PayloadCap::from_cap(zone::sign(RaceIdentity { id: 1 }).expect("first payload"));
        let second =
            PayloadCap::from_cap(zone::sign(RaceIdentity { id: 2 }).expect("second payload"));
        let binding = PublishedBinding::<RaceIdentity, PayloadCap<RaceIdentity>>::installed(first);
        let guard = epoch::guard();
        let mut second = Some(second);

        let retained = binding.retain_with_hook_for_test(&guard, |attempt| match attempt {
            0 => drop(binding.replace(second.take().expect("single replacement"))),
            1 => {}
            other => panic!("unexpected observation index {other}"),
        });

        assert_eq!(retained.expect("replacement remains non-empty").id, 2);
    }

    #[test]
    fn retained_read_metric_counts_public_calls_not_retries() {
        let _isolation = reset_race_zone();
        let before = RETAIN_ATTEMPTS_FOR_TEST.load(Ordering::Relaxed);
        let cap_binding = PublishedBinding::<RaceIdentity, Cap<RaceIdentity>>::installed(
            zone::sign(RaceIdentity { id: 1 }).expect("cap identity"),
        );
        let mut cap_second = Some(zone::sign(RaceIdentity { id: 2 }).expect("cap second"));
        let mut cap_third = Some(zone::sign(RaceIdentity { id: 3 }).expect("cap third"));
        let payload_binding = PublishedBinding::<RaceIdentity, PayloadCap<RaceIdentity>>::installed(
            PayloadCap::from_cap(zone::sign(RaceIdentity { id: 4 }).expect("payload identity")),
        );
        let mut payload_second = Some(PayloadCap::from_cap(
            zone::sign(RaceIdentity { id: 5 }).expect("payload second"),
        ));
        let mut payload_third = Some(PayloadCap::from_cap(
            zone::sign(RaceIdentity { id: 6 }).expect("payload third"),
        ));
        let guard = epoch::guard();

        assert!(cap_binding
            .retain_with_hook_for_test(&guard, |attempt| match attempt {
                0 => drop(cap_binding.replace(cap_second.take().expect("cap second"))),
                1 => drop(cap_binding.replace(cap_third.take().expect("cap third"))),
                other => panic!("unexpected cap observation index {other}"),
            })
            .is_some());
        assert!(payload_binding
            .retain_with_hook_for_test(&guard, |attempt| match attempt {
                0 => drop(payload_binding.replace(payload_second.take().expect("payload second"),)),
                1 => drop(payload_binding.replace(payload_third.take().expect("payload third"),)),
                other => panic!("unexpected payload observation index {other}"),
            })
            .is_some());
        assert_eq!(RETAIN_ATTEMPTS_FOR_TEST.load(Ordering::Relaxed) - before, 2);
    }
}
