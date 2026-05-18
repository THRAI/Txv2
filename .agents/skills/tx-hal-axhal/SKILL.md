---
name: tx-hal-axhal
description: Use when editing HAL, boot, pmap, trap, page-substrate, platform, or old OSTD-alignment docs.
---

# tx-hal-axhal

Use this skill for HAL/substrate boundary work.

## Read First

- `docs/design/01_substrate/HAL_v1.md`
- `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/00_meta-framework/INVARIANTS_v4.md` — MAP-2A and HAL rows (v5 carries these forward; cite v4 anchors since the other substrate docs do too).
- `docs/Txv3/02_INVARIANTS_v5.md` — read only if your change adds or rewords an enforceable rule; v5 is where new rows belong.

## Preserve

- HAL is an axHal-style static platform family.
- One platform is selected at compile/link time.
- No runtime HAL-manager type.
- No boxed dynamic HAL trait object.
- No `__ostd_main`.
- No HAL-owned semantic entities.
- No HAL callback slot for subsystem policy.

## Boundary

- HAL owns platform boot, traps, low-level hardware facts, pmap primitive surface, timer/IRQ/console traits.
- Page substrate owns frame allocator, `FrameMeta`, slab heap, and steady-state page accounting.
- Semantic subsystems own user-visible entities and policy.

## Debugging

- For RV64 QEMU `scause`/`sepc`/`stval` dumps, use `cargo xtask fault-decode
  --target rv64-qemu` before manual `nm`/`addr2line` work. It is a host-side
  decoder for trap logs and raw addresses; it does not change HAL trap policy.

## Done Means

- Old OSTD terms appear only as historical/migration/negative-rule notes.
- `PAGE_SUBSTRATE_v1.md` preconditions match `HAL_v1.md` H0-H4 boot sequence.
- No active doc describes HAL as a runtime subsystem or service manager.
