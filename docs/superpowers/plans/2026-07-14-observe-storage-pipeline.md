# Observe Storage Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a completed live observe run retain compressed raw evidence and Parquet by default, while reporting its local and aggregate storage use.

**Architecture:** L4 writes a temporary raw-record stream, then atomically replaces it with a gzip archive and records its integrity metadata. Python L4 reads either legacy raw input or gzip transparently; L6 remains the only Parquet owner. The OSComp workflow shares derived caches, removes copied build inputs after provenance is sealed, and prints a one-line storage accounting.

**Tech Stack:** Rust `flate2`, Python standard-library `gzip`, existing DuckDB Parquet export, `unittest`.

---

### Task 1: Compressed L4 capture

**Files:**
- Modify: `tools/tx-trace-daemon/Cargo.toml`
- Modify: `tools/tx-trace-daemon/src/l4_readers/live.rs`
- Test: `tools/tx-trace-daemon/src/l4_readers/live.rs`

- [x] **Step 1: Write failing Rust tests for gzip capture metadata and round trip.**
- [x] **Step 2: Run `cargo test --manifest-path tools/tx-trace-daemon/Cargo.toml live -- --nocapture` and confirm the missing gzip helper fails.**
- [x] **Step 3: Add a post-drain `GzEncoder` helper which writes `trace.rawrecords.gz` through a temporary sibling and renames it only after `finish()`. Record encoding, compressed/uncompressed byte counts, and SHA-256 in `runtime.json`.**
- [x] **Step 4: Re-run the focused daemon test and confirm it passes.**

### Task 2: Python transparent reader and default Parquet output

**Files:**
- Modify: `tools/tx_observe_host/l4_readers/__init__.py`
- Modify: `tools/tx-observe-analyze.py`
- Test: `tools/tests/test_tx_observe_analyze.py`

- [x] **Step 1: Write failing tests that decode a `.rawrecords.gz` fixture and invoke the analyzer without `--parquet-dir`.**
- [x] **Step 2: Run `python3 -m unittest tools.tests.test_tx_observe_analyze` and confirm failures identify gzip input and the absent default Parquet directory.**
- [x] **Step 3: Select `gzip.open` by suffix in L4 and derive a sibling `<input-name>.parquet` directory when no SQL/Python action or explicit Parquet directory is supplied.**
- [x] **Step 4: Re-run the focused Python suite and confirm it passes.**

### Task 3: Lean OSComp retention and storage accounting

**Files:**
- Modify: `tools/oscomp-observe-live.py`
- Test: `tools/tests/test_oscomp_observe_live.py`

- [x] **Step 1: Write failing tests for shared cache placement, gzip raw input, sealed provenance, and a deterministic one-line storage report.**
- [x] **Step 2: Run `python3 -m unittest tools.tests.test_oscomp_observe_live` and confirm the expected assertions fail.**
- [x] **Step 3: Route cache to `target/tx-observe/cache`, seal `capture.json` with run provenance before deleting copied `build`, `data`, and `submit` inputs, and emit `observe storage: run=... total=... captures=... cache=...`. Count allocated file blocks across recognized live-capture directories plus the shared cache.**
- [x] **Step 4: Re-run the OSComp workflow test and confirm it passes.**

### Task 4: Schema, documentation, and verification

**Files:**
- Modify: `schema/txobserve.toml`
- Modify: `docs/Txv3/08_OBSERVATION_HOST_v0.md`
- Modify: `docs/Txv3/08_OBSERVATION_L0_L6_REFACTOR_v0.md`
- Modify: `docs/progress/STATUS.md`

- [x] **Step 1: Describe gzip raw capture as an L4 input, Parquet as the default L6 materialization, and the precise aggregate-storage scope.**
- [x] **Step 2: Run `cargo xtask observe-schema check` and `cargo xtask observe-schema codegen --check`; correct any schema/catalog mismatch.**
- [x] **Step 3: Run the focused daemon and Python tests, `cargo xtask progress validate`, `cargo xtask lint docs`, and scoped `git diff --check`.**
- [x] **Step 4: Update `STATUS.md` with changed behavior, verification, and the absence of kernel-side ABI changes.**
