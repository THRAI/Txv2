# I/O Manager 6A Neutral Write IR Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a neutral, write-capable backend planning IR that represents leased PC/direct-I/O data sources and ordered bio dependencies without executing a concrete filesystem or driver path.

**Architecture:** `fs_iface::plan` owns pure values: source lease ids, data sources, graph nodes, graph edges, and resume payloads. `io_manager::backend::plan` only re-exports and transports the new graph through `BackendDispatch`; L4 does not submit the graph to L6 in this slice. Existing `Complete`, `SubmitBios`, and `MetadataFirst` variants remain unchanged compatibility paths.

**Tech Stack:** Rust `no_std` plus `alloc`, existing `BioPlan`/`BioVec`, `cargo test -p tx-subsystems --lib`.

---

## Canonical Gate

| Item | Authority | Existing integration point |
|---|---|---|
| `IoDataLeaseId` and `IoDataSource` | approved write-path spec §Required IR changes | `crates/tx-subsystems/src/fs_iface/plan.rs` |
| `BackendBioGraph` and dependencies | approved write-path spec §Required IR changes | `BackendPlan` in `fs_iface::plan` |
| graph transport only | approved write-path spec §Migration slice 6A | `BackendDispatch` in `io_manager/backend/plan.rs` |
| no concrete execution | `IO_MANAGER_v1.md` ownership rule | `PageService` remains unchanged except exhaustive enum transport |

## File Map

- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
  - Neutral source, graph, resume values, `BackendPlan::SubmitGraph`, default
    planner resume method, and isolated unit tests.
- Modify: `crates/tx-subsystems/src/io_manager/backend/plan.rs`
  - Re-export the new neutral values and map `SubmitGraph` to
    `BackendDispatch::BlockGraph`.
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
  - Reject `BackendDispatch::BlockGraph` with `ENOSYS`; do not flatten it into
    ordinary bios before the dependency scheduler exists.
- Create: `docs/superpowers/plans/2026-07-13-io-manager-6a-neutral-write-ir.md`
  - This implementation plan.

### Task 1: Define source and graph values

**Files:**
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Test: `crates/tx-subsystems/src/fs_iface/plan.rs`

- [x] **Step 1: Write the failing source-preservation test**

```rust
#[test]
fn backend_page_request_keeps_direct_write_source() {
    let source = IoDataSource::direct(
        IoDataLeaseId::new(7),
        alloc::vec![BioVec::new(9, 128, 512)],
    );
    let request = BackendPageRequest::new_with_source(
        FsObjectKey::new(2),
        PageIoRequestId::new(3),
        PageIoRange::new(0, 1),
        PageIoOp::Writeback,
        PageIoFlags::WRITEBACK,
        Some(PageGeneration::new(4)),
        source.clone(),
    );
    assert_eq!(request.source, source);
}
```

- [x] **Step 2: Run the failing test**

Run: `cargo test -p tx-subsystems --lib backend_page_request_keeps_direct_write_source -- --exact --nocapture`

Expected: compilation fails because `IoDataSource`, `IoDataLeaseId`, and
`BackendPageRequest::new_with_source` do not exist.

- [x] **Step 3: Add the minimal source values**

```rust
pub struct IoDataLeaseId(u64);

pub enum IoDataSource {
    None,
    PageCache { lease: IoDataLeaseId, frame: PageFrameRef, offset: u32, len: u32 },
    Direct { lease: IoDataLeaseId, vecs: Vec<BioVec> },
}
```

Add `BackendPageRequest::new_with_source(...)`; retain `new` and
`from_page_io_request` with `IoDataSource::None` so every existing planner
continues to compile.

- [x] **Step 4: Run the source test**

Run: `cargo test -p tx-subsystems --lib backend_page_request_keeps_direct_write_source -- --exact --nocapture`

Expected: PASS.

### Task 2: Make ordered graph validity explicit

**Files:**
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Test: `crates/tx-subsystems/src/fs_iface/plan.rs`

- [x] **Step 1: Write failing graph tests**

```rust
#[test]
fn backend_bio_graph_rejects_a_cycle() {
    let graph = BackendBioGraph::new(
        alloc::vec![node(1), node(2)],
        alloc::vec![BackendBioDependency::new(node_id(1), node_id(2)),
                    BackendBioDependency::new(node_id(2), node_id(1))],
    );
    assert_eq!(graph, Err(BackendBioGraphError::Cycle));
}

#[test]
fn backend_bio_graph_keeps_data_before_commit_dependency() {
    let graph = BackendBioGraph::new(
        alloc::vec![node(1), node(2)],
        alloc::vec![BackendBioDependency::new(node_id(1), node_id(2))],
    ).expect("acyclic graph");
    assert_eq!(graph.dependencies().len(), 1);
}
```

- [x] **Step 2: Run the failing graph tests**

Run: `cargo test -p tx-subsystems --lib backend_bio_graph_ -- --nocapture`

Expected: compilation fails because graph values and validation do not exist.

- [x] **Step 3: Add graph values and validation**

```rust
pub struct BackendBioNodeId(u64);
pub struct BackendBioNode { pub id: BackendBioNodeId, pub bio: BioPlan, pub source: IoDataSource }
pub struct BackendBioDependency { pub before: BackendBioNodeId, pub after: BackendBioNodeId }
pub struct BackendBioGraph { nodes: Vec<BackendBioNode>, dependencies: Vec<BackendBioDependency> }
pub enum BackendBioGraphError { DuplicateNode, UnknownNode, SelfDependency, Cycle }
```

`BackendBioGraph::new` rejects duplicate ids, missing endpoints, self edges,
and cycles. The implementation uses bounded `Vec` scans and DFS colors; it
does not add an index or scheduler. Add `BackendPlan::SubmitGraph` only after
the graph constructor has tests.

- [x] **Step 4: Run graph tests**

Run: `cargo test -p tx-subsystems --lib backend_bio_graph_ -- --nocapture`

Expected: PASS.

### Task 3: Transport graphs and resumptions without executing them

**Files:**
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Modify: `crates/tx-subsystems/src/io_manager/backend/plan.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
- Test: `crates/tx-subsystems/src/io_manager/backend/plan.rs`
- Test: `crates/tx-subsystems/src/io_manager/page/service.rs`

- [x] **Step 1: Write failing dispatch test**

```rust
#[test]
fn dispatch_backend_plan_preserves_submit_graph() {
    let graph = test_graph();
    assert_eq!(
        dispatch_backend_plan(BackendPlan::SubmitGraph(graph.clone())),
        BackendDispatch::BlockGraph(graph),
    );
}
```

- [x] **Step 2: Run the failing dispatch test**

Run: `cargo test -p tx-subsystems --lib dispatch_backend_plan_preserves_submit_graph -- --exact --nocapture`

Expected: compilation fails because `SubmitGraph` and `BlockGraph` do not
exist.

- [x] **Step 3: Add transport and resume compatibility**

Add `BackendPlanResume { token, completed }` and
`BackendPlanner::resume_page_io`, whose default returns `BackendPlan::Err(ENOSYS)`.
Re-export these values, add `BackendDispatch::BlockGraph`, and map
`BackendPlan::SubmitGraph` directly. Page service must reject a graph with
`ENOSYS` rather than flattening or reordering it.

- [x] **Step 4: Run dispatch and page-service tests**

Run: `cargo test -p tx-subsystems --lib dispatch_backend_plan_preserves_submit_graph -- --exact --nocapture`

Run: `cargo test -p tx-subsystems --lib io_manager -- --nocapture`

Expected: PASS. Existing `SubmitBios` and `MetadataFirst` tests stay green.

### Task 4: Verify neutral boundary and record progress

**Files:**
- Modify: `docs/progress/plans/2026-07-13-ext4-io-manager-write-path.json`
- Modify: `docs/progress/STATUS.md`

- [x] **Step 1: Run focused verification**

Run: `cargo test -p tx-subsystems --lib io_manager -- --nocapture`

Run: `cargo check -p tx-subsystems`

Run: `rg -n "tx_ext4|tx-ext4|tx_fs|tx-fs|bdevfs|BlockDeviceHandle|read_blocks|write_blocks" crates/tx-subsystems/src/io_manager crates/tx-subsystems/src/fs_iface`

Expected: tests and check pass; the boundary scan finds no concrete backend or
driver dependency in the new IR.

- [x] **Step 2: Update progress**

Mark `6a-neutral-write-ir` complete only after the verification output is
recorded. Add a concise `STATUS.md` entry with the source/graph/resume contract,
commands run, and the next slice (`6B`).

- [x] **Step 3: Validate records and whitespace**

Run: `cargo xtask progress validate`

Run: `git diff --check -- crates/tx-subsystems/src/fs_iface crates/tx-subsystems/src/io_manager docs/progress`

Expected: PASS.
