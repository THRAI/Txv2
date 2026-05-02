# 2026-05-02: VM `PmapIf` root owner

## Decision

`AddressSpace` stays the zone-owned semantic VM entity exposed as
`Cap<AddressSpace>`, while the VM pmap field becomes a private `VmPmap` owner.
`VmPmap` creates a HAL `PmapRoot`, records its ASID, and captures a root-local
table of monomorphized `PmapIf` functions for reserve, commit, unmap, shootdown,
and root destruction.

This is ownership glue, not a runtime HAL manager: no public API exposes raw
`Zone<T, Policy>`, no boxed HAL trait object exists, no platform is selected at
runtime, and HAL still owns only pmap mechanics.

## Rationale

The active VM docs require `AddressSpace` to remain a non-generic semantic
entity retained through zone-derived evidence, but the pmap root must be
created and destroyed through the statically selected platform. The private
root-local function table lets the non-generic zone entity reclaim its platform
root while keeping the static `PmapIf` boundary intact.

VM keeps a shadow map only to own Rust `MapPin` evidence for committed PTEs.
Hardware PTEs remain derived materializations justified by recipes; the shadow
map is not authoritative VM policy state.

## Verification

Initial focused verification during the implementation pass:

- `cargo test -p tx-kernel vm`

Full verification remains tracked in
`docs/progress/worktrees/2026-05-02-vm-pagebacked-impl.json`.
