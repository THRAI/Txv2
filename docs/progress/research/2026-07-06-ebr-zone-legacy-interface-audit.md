# EBR/Zone legacy interface audit

Date: 2026-07-06

Scope: audit live EBR/Zone interface residue against
`docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md` and add a repeatable lint
for production Rust code.

## Interface Boundary

The current object-model surface is:

- `epoch::guard()` and `Guard`;
- `Weak<T>`, `IdentRef<'g, T>`, `Cap<T>`, `PayloadCap<T>`;
- `ZoneReservation<T>`, `zone::reserve`, and `zone::sign`;
- hidden policy selection in substrate/entity-zone declarations, not in
  semantic operation code.

The legacy/imported spellings audited were `epoch::pin`, `EpochGuard`,
`WeakCap`, `weak_count`, `ReservedSlot`, `ReservedSlot::init`, `Cap::get`,
`RcPolicy`, `EbrPolicy`, and raw `Zone<T, Policy>` in upper code.

## Findings

Production Rust code now has zero legacy EBR/Zone interface sites under the new
ratchet:

```sh
cargo xtask lint invariants zone-interface
# legacy EBR/Zone interface sites: 0 (ceiling 0)
```

The one remaining targeted `rg` hit in live Rust is a comment in
`crates/tx-substrate/src/zone/policy.rs` that explains why the public API
exposes `Zone<T>` rather than `Zone<T, Policy>`; the lint strips comments and
does not count it.

The audit found three stale active-design examples using `epoch::pin()`:

- `docs/design/03_memory-vm/VM_v1_2.md`;
- `docs/design/05_filesystem/VFS_CHECKS_V2.1.md` twice.

Those examples were updated to `epoch::guard()`. The VFS example also now uses
`let mut outer_guard` because the pseudo-code reassigns the guard after an I/O
yield.

Intentional historical mentions remain in the EBR/Zone adaptation and invariant
documents where they describe the old-to-new mapping, for example
`EBR_ZONE_INTERFACE_v1.md` and `INVARIANTS_v4.md`. They are documentation of the
migration rule, not live interface use.

`Tombstone` was not included as a raw lint token because it is not unique to the
old EBR interface: the HAL page-table code has a legitimate
`CommittedPtNodeEntry::Tombstone` state unrelated to zone reclamation. The new
lint focuses on unambiguous legacy API spellings.

## New Lint

Added `cargo xtask lint invariants zone-interface`.

The lint scans production Rust source in `boards/` and the main runtime crates,
skips tests and comments, reports counts by rule, and fails if any legacy site
appears. It allows hidden policy internals inside `crates/tx-substrate/src/zone`
and zone manifest files while rejecting policy selection in upper code.

The lint is also wired into `cargo xtask lint invariants all`.

## Verification

- `cargo xtask lint invariants zone-interface` passed with 0 sites.
- `cargo test -p xtask lint_invariants_zone -- --nocapture` passed 4/4 tests.
- `cargo fmt --check -p xtask` passed.
- `git diff --check -- xtask/src/lint_invariants_zone.rs xtask/src/lint.rs xtask/src/lib.rs docs/design/03_memory-vm/VM_v1_2.md docs/design/05_filesystem/VFS_CHECKS_V2.1.md` passed.
- `cargo xtask lint docs` passed; it still reports the pre-existing
  stale-vocabulary warning class for active docs that discuss retired terms.

## Next Step

Keep the `zone-interface` ratchet at zero. If future work introduces a genuine
zone-policy implementation detail, keep it inside substrate or an entity-zone
manifest rather than raising the ceiling.

Blocker: none.
