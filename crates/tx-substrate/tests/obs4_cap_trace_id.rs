//! OBS-4: `Cap::trace_id()` bit-layout test.
//!
//! Verifies the packed `trace_id` layout:
//!
//! ```text
//! bits  0..32  — slot index (zone_id in upper 8 of 32, slot_id in lower 24)
//! bits 32..56  — generation (u16 from slot metadata, zero-extended into 24 bits)
//! bits 56..64  — kind discriminant: FNV-1a hash of TypeId<T>, low 8 bits
//! ```
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-4, §12 ObjectId.
//!
//! One test function covers all assertions to avoid Zone id-allocation races
//! between parallel test runs (each Zone<T> static caches its ZoneId; a reset
//! does not clear those caches, so two tests must never allocate ids for the
//! same type in parallel).

extern crate std;

use tx_substrate::zone::{self, Cap, PayloadCap, Zone, ZoneAllocated};
use tx_substrate::epoch;

static ZONE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Test object types ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct AlphaObj {
    _v: u32,
}

#[derive(Debug)]
struct BetaObj {
    _v: u64,
}

static ALPHA_OBJ_ZONE: Zone<AlphaObj> = Zone::const_new();
static BETA_OBJ_ZONE: Zone<BetaObj> = Zone::const_new();

unsafe impl ZoneAllocated for AlphaObj {
    fn zone() -> &'static Zone<Self> {
        &ALPHA_OBJ_ZONE
    }
}

unsafe impl ZoneAllocated for BetaObj {
    fn zone() -> &'static Zone<Self> {
        &BETA_OBJ_ZONE
    }
}

// ── Setup helper ──────────────────────────────────────────────────────────────

fn reset_and_init() -> std::sync::MutexGuard<'static, ()> {
    let guard = ZONE_LOCK.lock().expect("zone lock");
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    epoch::testing::init_for_test();
    zone::testing::init_for_test(
        4096,
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("zone init");
    guard
}

// ── Single test covering all assertions ──────────────────────────────────────

/// Verifies all `Cap::trace_id()` and `PayloadCap::trace_id()` properties in
/// one test to avoid id-allocation races when tests run in parallel.
///
/// Properties checked:
/// 1. Bit layout: `(kind<<56) | (generation<<32) | slot_raw` reconstructs the id.
/// 2. The slot component equals `cap.raw()`.
/// 3. The generation component equals what `cap.downgrade().generation()` reports.
/// 4. Same type → same kind byte.
/// 5. Different types → different kind bytes.
/// 6. `PayloadCap::trace_id()` delegates to `Cap::trace_id()`.
/// 7. The id is deterministic for a live cap.
#[test]
fn cap_trace_id_properties() {
    let _guard = reset_and_init();

    // Register both zones in a single session to guarantee stable ids.
    zone::register_zone_for::<AlphaObj>().expect("register alpha zone");
    zone::register_zone_for::<BetaObj>().expect("register beta zone");

    // ── Allocate two AlphaObj caps and one BetaObj cap ───────────────────────
    let a1: Cap<AlphaObj> = zone::sign_for(
        zone::reserve_for::<AlphaObj>().expect("reserve alpha 1"),
        AlphaObj { _v: 1 },
    );
    let a2: Cap<AlphaObj> = zone::sign_for(
        zone::reserve_for::<AlphaObj>().expect("reserve alpha 2"),
        AlphaObj { _v: 2 },
    );
    let b: Cap<BetaObj> = zone::sign_for(
        zone::reserve_for::<BetaObj>().expect("reserve beta"),
        BetaObj { _v: 99 },
    );

    // ── Property 1–3: bit layout for a1 ─────────────────────────────────────
    let id_a1 = a1.trace_id();
    let slot_a1: u32 = id_a1 as u32;
    let gen_a1: u16 = ((id_a1 >> 32) & 0xffff) as u16;
    let kind_a1: u8 = (id_a1 >> 56) as u8;

    // Slot component must equal cap.raw().
    assert_eq!(slot_a1, a1.raw(), "trace_id bits 0..32 must equal cap.raw()");

    // Generation component must equal what downgrade() reports.
    assert_eq!(
        gen_a1,
        a1.downgrade().generation(),
        "trace_id bits 32..48 must equal the slot generation"
    );

    // Reconstruction formula must hold.
    let reconstructed =
        ((kind_a1 as u64) << 56) | ((gen_a1 as u64) << 32) | (slot_a1 as u64);
    assert_eq!(
        reconstructed, id_a1,
        "formula (kind<<56)|(gen<<32)|slot must reconstruct trace_id"
    );

    // ── Property 7: determinism ──────────────────────────────────────────────
    assert_eq!(
        a1.trace_id(),
        id_a1,
        "trace_id must be deterministic for a live cap"
    );

    // ── Property 4: same type → same kind byte ───────────────────────────────
    let kind_a2: u8 = (a2.trace_id() >> 56) as u8;
    assert_eq!(
        kind_a1, kind_a2,
        "two Cap<AlphaObj> must share the same kind byte"
    );

    // ── Property 5: different types → different kind bytes ───────────────────
    let kind_b: u8 = (b.trace_id() >> 56) as u8;
    assert_ne!(
        kind_a1, kind_b,
        "Cap<AlphaObj> and Cap<BetaObj> should have distinct kind bytes"
    );

    // ── Property 6: PayloadCap::trace_id delegates to Cap::trace_id ─────────
    let id_from_cap = a1.trace_id();
    let payload_cap = PayloadCap::from_cap(a1);
    assert_eq!(
        id_from_cap,
        payload_cap.trace_id(),
        "PayloadCap::trace_id() must equal Cap::trace_id()"
    );
}
