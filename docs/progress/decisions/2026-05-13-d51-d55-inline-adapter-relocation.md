# D51-D55: Inline Adapter Relocation (aio, signalfd, userfaultfd, io_uring, reactor_submit)

**Date:** 2026-05-13  
**Phase:** 7/8 boundary burndown  
**Status:** Complete

## Problem

Five files in `crates/tx-subsystems/src/` defined their `#[platform_adapter]` modules inline inside the same file that consumed them:

- `aio.rs` (pub mod adapter)
- `signalfd.rs` (pub mod adapter)
- `userfaultfd.rs` (pub mod adapter)
- `io_uring.rs` (pub mod adapter)
- `reactor_submit.rs` (mod adapter — private)

The boundary scanner classifies a file as inside-adapter if it contains `#[platform_adapter]`. With inline adapters, the entire file — including the consumer logic and test-bootstrap residue — counted as inside-adapter. This hid real outside-adapter call sites from the ratchet gate.

## Decision

Extract each inline `mod adapter { ... }` block into a sibling `adapter.rs` file by converting `<name>.rs` → `<name>/mod.rs` + `<name>/adapter.rs`.

**Pattern (for each file `<name>.rs`):**

1. `mkdir crates/tx-subsystems/src/<name>`
2. `git mv crates/tx-subsystems/src/<name>.rs crates/tx-subsystems/src/<name>/mod.rs`
3. Create `crates/tx-subsystems/src/<name>/adapter.rs` containing only the body of the inline `mod adapter { }` block (without the wrapping declaration)
4. In `mod.rs`, replace `pub mod adapter { ... }` with `pub mod adapter;` (or `mod adapter;` if the original was private)
5. All existing `use adapter::...` imports in `mod.rs` continue to resolve identically

**Visibility preservation:**
- `aio`, `signalfd`, `userfaultfd`, `io_uring` had `pub mod adapter` → extracted as `pub mod adapter;`
- `reactor_submit` had `mod adapter` (private) → extracted as `mod adapter;`

## Effect on Boundary Counts

Before D51-D55: inline adapter → file classified as inside-adapter → test-bootstrap residue hidden.

After D51-D55: `adapter.rs` classified as inside-adapter, `mod.rs` classified as outside-adapter with only test-bootstrap residue (`init_host_for_test_once`, `drain_with_budget`).

| File | Outside lines before | Outside lines after |
|------|---------------------|---------------------|
| aio | 0 (whole file inside) | 3 (test residue only) |
| signalfd | 0 (whole file inside) | 3 (test residue only) |
| userfaultfd | 0 (whole file inside) | 3 (test residue only) |
| io_uring | 0 (whole file inside) | 4 (1 doc comment + 3 test residue) |
| reactor_submit | 0 (whole file inside) | 0 (no substrate refs in mod.rs) |

**Net effect:** +13 newly visible outside lines (test-bootstrap residue previously hidden inside the adapter-classified files). These lines are sanctioned residue — the pattern is correct.

## Ratchet Ceiling

D50 had lowered `MAX_SUBSTRATE_OUTSIDE_ADAPTER` from 208 → 192 based on its measurement with D51-D53 already in tree. D54-D55 added 4+0 = 4 more newly-visible residue lines, raising the effective count from 192 → 199 (D54's io_uring/mod.rs has 1 doc-comment line and 3 test-bootstrap lines; reactor_submit/mod.rs has 0).

A concurrent `fixup D50` commit adjusted the ceiling from 192 → 199 to reflect the actual post-D54/D55 workspace state.

**Final ceiling: 199** (`MAX_SUBSTRATE_OUTSIDE_ADAPTER = 199`)

`cargo xtask lint boundary` passes at ceiling 199 with actual count 199.

## Commits

| Commit | Hash | Change |
|--------|------|--------|
| D51 | d049833 | aio.rs → aio/mod.rs + aio/adapter.rs |
| D52 | 9d74aba | signalfd.rs → signalfd/mod.rs + signalfd/adapter.rs |
| D53 | 3a9f821 | userfaultfd.rs → userfaultfd/mod.rs + userfaultfd/adapter.rs |
| D54 | 8b99513 | io_uring.rs → io_uring/mod.rs + io_uring/adapter.rs |
| D55 | 21480d1 | reactor_submit.rs → reactor_submit/mod.rs + reactor_submit/adapter.rs |

## Pitfalls Noted

- The `lib.rs` `pub mod aio;` / `pub mod signalfd;` etc. declarations continue to work for `<name>/mod.rs` — no change needed.
- Files where the inline adapter had `use` statements that scope items: those `use` statements must be reproduced inside `adapter.rs` at the top level (not as module-level imports inside a nested mod).
- The scanner classifies a whole file based on whether it contains `#[platform_adapter]`. After the split, `mod.rs` loses the attribute; its substrate refs become outside-adapter — these are expected residue lines.
- Visibility of the extracted file declaration (`pub mod` vs `mod`) must match the original inline block visibility.

## Verification

Each commit verified with:
- `cargo build -p tx-subsystems` (clean, no errors)
- `cargo test -p tx-subsystems --lib <name> -- --test-threads=1` (all tests pass)
- `rg "tx_substrate::" crates/tx-subsystems/src/<name>/mod.rs` shows only test-bootstrap residue
