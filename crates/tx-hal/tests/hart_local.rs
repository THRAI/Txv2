//! Integration tests for `HartLocal<T>`.
//!
//! Uses a minimal `MockPercpu` that pins the "current hart" to a compile-time
//! constant, letting us drive `get` without a real scheduler.

use tx_hal::{CpuId, HartLocal, PercpuIf, MAX_HARTS};

// ---------------------------------------------------------------------------
// Minimal mock — pins current hart to hart 0
// ---------------------------------------------------------------------------

struct Hart0;
impl PercpuIf for Hart0 {
    fn current_cpu_id() -> CpuId {
        CpuId(0)
    }
}

struct Hart1;
impl PercpuIf for Hart1 {
    fn current_cpu_id() -> CpuId {
        CpuId(1)
    }
}

// ---------------------------------------------------------------------------
// Test 1: const-constructible; usable in a `static`
// ---------------------------------------------------------------------------

static STATIC_SLOT: HartLocal<u32> = HartLocal::new();

#[test]
fn hart_local_can_be_used_in_static() {
    // If this compiles and the static is reachable, the const constructor works.
    // Touch it to suppress dead-code lint.
    let _ = STATIC_SLOT.get::<Hart0>();
}

// ---------------------------------------------------------------------------
// Test 2: get before init returns None
// ---------------------------------------------------------------------------

#[test]
fn get_before_init_returns_none() {
    static SLOT: HartLocal<u64> = HartLocal::new();
    assert!(SLOT.get::<Hart0>().is_none());
}

// ---------------------------------------------------------------------------
// Test 3: init then get returns the value
// ---------------------------------------------------------------------------

#[test]
fn init_then_get_returns_value() {
    static SLOT: HartLocal<u32> = HartLocal::new();
    SLOT.init(CpuId(0), 42u32);
    assert_eq!(SLOT.get::<Hart0>(), Some(&42u32));
}

// ---------------------------------------------------------------------------
// Test 4: two HartLocals don't alias
// ---------------------------------------------------------------------------

#[test]
fn two_hart_locals_do_not_alias() {
    static SLOT_A: HartLocal<u32> = HartLocal::new();
    static SLOT_B: HartLocal<u32> = HartLocal::new();

    SLOT_A.init(CpuId(0), 1u32);
    SLOT_B.init(CpuId(0), 2u32);

    assert_eq!(SLOT_A.get::<Hart0>(), Some(&1u32));
    assert_eq!(SLOT_B.get::<Hart0>(), Some(&2u32));
}

// ---------------------------------------------------------------------------
// Test 5: different harts get independent slots
// ---------------------------------------------------------------------------

#[test]
fn different_harts_get_independent_slots() {
    static SLOT: HartLocal<u32> = HartLocal::new();

    SLOT.init(CpuId(0), 10u32);
    SLOT.init(CpuId(1), 20u32);

    assert_eq!(SLOT.get::<Hart0>(), Some(&10u32));
    assert_eq!(SLOT.get::<Hart1>(), Some(&20u32));
}

// ---------------------------------------------------------------------------
// Test 6: get on uninitialised hart returns None, even after another is set
// ---------------------------------------------------------------------------

#[test]
fn get_on_uninitialised_hart_returns_none_after_other_init() {
    static SLOT: HartLocal<u32> = HartLocal::new();

    SLOT.init(CpuId(0), 99u32);
    // Hart 1 not initialised.
    assert!(SLOT.get::<Hart1>().is_none());
}

// ---------------------------------------------------------------------------
// Test 7: MAX_HARTS is 64 and matches CpuMask bit-width
// ---------------------------------------------------------------------------

#[test]
fn max_harts_is_64() {
    assert_eq!(MAX_HARTS, 64);
}

// ---------------------------------------------------------------------------
// Test 8: Send + Sync compile-time check
// ---------------------------------------------------------------------------

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn hart_local_u32_is_send_sync() {
    assert_send_sync::<HartLocal<u32>>();
}
