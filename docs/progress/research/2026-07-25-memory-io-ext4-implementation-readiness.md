# Memory/I-O and ext4 implementation-readiness audit

Date: 2026-07-25

## Verdict

**Ready: no.** `MEMORY_IO_ARCHITECTURE_v1.md` is a coherent target contract,
but the current ext4/PageBacked checkout is a staged implementation. Existing
BIO graphs, typed sources, adjacent-LBA merge, direct-I/O pins, and ordered
JBD2 graph structure are useful foundations; they do not close the ownership,
reclaim, transaction, or crash-consistency obligations below.

## Blocking gaps

| Priority | Claim | Current evidence | Exit gate |
|---|---|---|---|
| P1 | ext4 file PCs are neither canonical nor in the clean-reclaim registry | `crates/tx-ext4/src/namespace.rs:382-415` creates every file PC with `PageContainer::new_cap`; only `new_file_cap` registers a weak reclaim row at `crates/tx-subsystems/src/page_backed/mod.rs:1023-1045`; the service runtime holds a strong PC and the production registry has no retire path at `crates/tx-subsystems/src/device.rs:666-737` | mount-scoped `(mount, object) -> Weak<PC>` find-or-create, idempotent service binding, explicit retirement, duplicate materialization/lifetime tests |
| P1 | current clean reclaim can leave a stale `Resident` slot | `PageCacheIndex::reclaim_clean_pages` removes only index entries at `crates/tx-subsystems/src/page_backed/mod.rs:363-381`; `PageSlot::complete_fetch` accepts only `Fetching` at `crates/tx-subsystems/src/page_backed/slot.rs:194-211` | one generation-checked owner withdrawal updates resident binding and slot; clean-reclaim-refetch/race tests |
| P1 | journal data/commit error can leak the ring reservation | error completion calls only `transaction.discard()` at `crates/tx-ext4/src/journal.rs:1299-1315` and `:1669-1684`; the stored reservation is taken/completed only after successful checkpoint at `:1342-1362` | exactly-once abort release and an error-then-next-reservation reuse test |
| P1 | multi-page fsync has no forward-progress proof | PageBacked submits each dirty frontier page separately at `crates/tx-subsystems/src/page_backed/mod.rs:1700-1735`; ext4 rejects `page_count != 1` at `crates/tx-ext4/src/read_backend.rs:62-68`; `JournalTransactionState::begin` rejects a second active transaction at `crates/tx-ext4/src/journal.rs:861-876` | aggregate a frontier into one transaction, or prove an equivalent serialized protocol; multi-page/redirty/error tests |
| P1 | namespace mutations bypass the admitted JBD2 graph | create/unlink call compatibility pager mutations at `crates/tx-ext4/src/namespace.rs:136-186`; rename performs destination remove, destination append, then source remove separately at `:189-248` | one mutation graph/transaction per supported operation; rename crash/replay old-or-new test |
| P1 | dirty authority remains duplicated | `PageCacheEntry::marks` includes dirty/writeback at `crates/tx-subsystems/src/page_backed/mod.rs:148-168`; writeback admission updates both slot and marks at `:1566-1604` | remove semantic dirty/writeback from replacement marks; static ratchet plus state-machine tests |
| P1 | global memory pressure control is absent | current production behavior is a fixed weak registry/clean sweep plus selected allocation retry; the four target interfaces have no production owner | land `AllocationGateway`, coordinator, pure policy, and providers only after owner reclaim correctness is proved |

## Precision note: `SealedDataWrite`

`SealedDataWrite` remains a copy-capable legacy DTO because it embeds
`Page4K` (`crates/tx-ext4-format/src/mutation.rs:43-48`). The current main
writeback planner passes a zero-filled placeholder and explicitly states that
the runtime replaces it with the L4-owned source
(`crates/tx-ext4/src/read_backend.rs:62-87`). Journal lowering accepts the
retained `IoDataSource` and builds `BioVec` directly from its frame
(`crates/tx-ext4/src/journal.rs:1208-1224`). Therefore the DTO must migrate to
an opaque lease slice, but its embedded field/placeholder is not by itself
evidence that the current normal payload path performs an extra 4-KiB copy.

## Implemented foundations

- Existing `BackendBioGraph` and L4/L6 execution are the correct extension
  point; no second graph is required.
- The main single-page aligned writeback path can bind an L4-owned page-cache
  source directly into a BIO.
- JBD2 planning already expresses ordered data, commit fences/FUA, and
  checkpoint graphs.
- Block submission already has adjacent-LBA merge, and direct I/O has typed
  pin/lifetime evidence.
- `tx-ext4-format` remains independent of PageBacked, PPN, `BioVec`, and the
  reactor.

## Required order

1. Canonical file-PC lifecycle and reclaim/`PageSlot` coherence.
2. Journal abort/reservation cleanup.
3. Multi-page fsync-frontier transaction.
4. Namespace mutation through the same JBD2 graph.
5. Multi-page lease and pure pager extraction.
6. Pressure coordinator and performance policy.

## Verification status

The review input reports 31 `tx-ext4` unit tests and 147 focused PageBacked
tests passing. Those suites do not cover clean-reclaim-refetch, multi-page
fsync, reservation reuse after I/O error, or namespace crash consistency. This
documentation audit did not rerun those component tests; its own doc/progress
checks are recorded in `docs/progress/STATUS.md`.
