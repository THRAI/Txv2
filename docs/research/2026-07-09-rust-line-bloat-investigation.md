---
date: 2026-07-09
topic: "Why the workspace is ~428k Rust lines, and what is reducible"
status: draft
scope:
  - crates/tx-subsystems
  - crates/tx-shims
  - crates/tx-substrate
  - external
---

# Research: Rust Line-Count Bloat Investigation

## Question

The workspace reports ~428k Rust lines. Where does that count actually live,
how much is genuine first-party production logic, and what fraction is
mechanically reducible without losing behavior or coverage?

## Short Answer

The headline 428k is misleading. Honest first-party production logic is
**~150k lines (~35%)**. The rest is tests (~38%), vendored third-party code
(~17%), and comments/docs (~10%). The workspace is large mostly because it
implements a broad, genuinely-emulated Linux ABI plus an equally large test
suite — not because of pervasive bloat. The compressible slack is real but
bounded: roughly **15–20% of first-party production code**, concentrated in a
few named areas, not a 2x reduction.

First correction for any reporting: quote **~357k first-party**, not 428k. The
`external/` tree (smoltcp-asterinas 55k + rsext4 16k) is vendored and should
not count toward our bloat.

## Findings

### The real pie of 428k

| Bucket | Est. lines | % | Verdict |
|---|---|---|---|
| Tests (out-of-line + inline `#[cfg(test)]`) | ~161,000 | ~38% | Largest category |
| Genuine production logic | ~150,000 | ~35% | The real kernel |
| Vendored external (smoltcp 55k + rsext4 16k) | ~71,000 | ~17% | Not ours |
| Comments/docs (first-party) | ~43,000 | ~10% | Reasonable for unsafe code |
| Error plumbing | <2,000 | <0.5% | Already lean |

### LOC by crate (first-party giants)

- `crates/tx-subsystems` — 136,406 (net 42,023, vm 20,458, vfs 10,704,
  process 9,076, tty 9,066, page_backed 7,665)
- `crates/tx-shims` — 69,886 (essentially all `linux_syscall/` 64,634)
- `crates/tx-substrate` — 28,567
- `crates/tx-kernel` — 17,066; `crates/tx-reactor` — 16,361;
  `crates/tx-fs` — 12,493
- `external/` — 70,954 (vendored)

### Where the compressible slack lives

**1. Tests are the single biggest lever (~38%).** Not bloat in the pejorative
sense — it is coverage — but it is where the line count sits, and it is written
in a way that inflates it. 634 hand-written `#[test]` fns in tx-shims, near-zero
use of parameterized/table-driven macros (only 1 file workspace-wide uses
rstest/test_case), and copy-pasted setup preambles: tmpfs preamble ×53, vm
`setup_host_substrate` ×52, net epoch-lock preamble at 179 sites. The shim tests
alone carry **1,427 bare `0,` padding lines** from 6-slot syscall arg arrays.
Reducible: **~25–35k lines** via fixtures/builders/table-driven cases with zero
coverage lost. Concentration points: `linux_syscall/tests` (27.3k) and
`net/tests` (14.3k) — together larger than all of tx-substrate.

**2. The Linux surface is broad and mostly essential.** `linux_syscall` is ~265
handlers implementing real TCP loopback, msghdr/cmsg assembly, iovec walking,
poll. `net` implements real socket state machines. Essential fraction estimated
at 54–60% in both. You cannot macro away emulation logic.

**3. Bounded reducible boilerplate — ~12–17k first-party production lines:**

- **net (~4–6.5k):** two hand-rolled netlink wire stacks (rtnetlink +
  nfnetlink) with `push_attr`/`align4`/`read_u32` duplicated verbatim across
  both; ~22 IPv4/IPv6 copy-paste twin functions; per-protocol
  `match SocketProtocol` fan-out re-matched in every `step_*` file (45 arms in
  step_connect alone); ARP/fragmentation reimplemented despite smoltcp providing
  it.
- **linux_syscall (~2–3k):** the
  `build_subject_script_ctx -> drive_oneshot -> Ok/Err(v3errno)` tail repeated
  in 50–65 handlers; dispatch table using `nr if nr == NR_X =>` guards (252 of
  them) instead of const patterns; hand-written errno map + 31 scattered
  `*_VALUE` consts.
- **object-model pattern tax (~3–4k):** zone plumbing hand-written 45x/49x
  despite an existing `stub_zone!` macro; SysV IPC shm/sem/msg triplicated
  byte-for-byte apart from renames; the same substrate re-export block copied
  into 19 `adapter.rs` files. See the companion note
  `2026-07-09-object-model-pattern-tax.md` for the deep dive.

### What is NOT a bloat source (do not target)

- **tx-substrate is not the problem.** Its reputation as generic-heavy is wrong:
  ~21% comments, unsafe-heavy (271 `unsafe`), but only ~564 lines of heavy
  generics. Weight is in step/zone/bus/wake (reactor-adjacent), not the pure
  data-structure primitives.
- **Error plumbing:** no `thiserror`, 1 total `Display` impl, dedicated error
  files total ~75 lines. Already minimal.
- **Macros and derives:** 21 `macro_rules!`, 1,191 derive lines — each well
  under 1%.

## Applicability To txKernel

- Report **~357k first-party** as the honest baseline; exclude `external/`.
- If line count is a concern, the payoff ranking is: (1) table-driven test
  consolidation (biggest raw number), (2) `#[derive(ZoneAllocated)]` proc-macro
  for the zone plumbing tax, (3) unify the two netlink wire stacks, (4) a
  `syscall!` macro for the StepOp handler tail.
- Do **not** spend effort on tx-substrate, error enums, macros, or derives —
  each is already lean.
- The 1,591 `Cap<>` sites and 183 `StepOp` transitions are load-bearing
  semantics, not tax.

## Method / Caveats

- Line counts via `wc -l` over `*.rs` excluding `target/`; categories estimated
  by grep sampling of the heaviest files, not an exhaustive per-line classifier,
  so bucket totals are approximate (±a few percent).
- Fan-out of five parallel readers over net, linux_syscall, object model, tests,
  and substrate/cross-cutting. Findings are read-only; no files were edited.

## Sources

- Companion deep dive: `docs/research/2026-07-09-object-model-pattern-tax.md`
- Key files: `crates/tx-subsystems/src/net/`,
  `crates/tx-shims/src/linux_syscall/`, `crates/tx-substrate/src/`
