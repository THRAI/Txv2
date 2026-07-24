#!/usr/bin/env python3
"""Summarize tx-observe replay timing from NDJSON or raw txtrace.

The analyzer accepts either the JSON stream produced by:

    cargo xtask observe replay --file trace.txtrace --out json

or a raw `.txtrace` file via `--file`. It pairs SpanBegin/SpanEnd records,
aggregates syscall/drive duration, and prints the largest inter-record gaps.
The report is deliberately text-first so it can be pasted into progress notes
or debugging handoffs.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

TOOLS_DIR = Path(__file__).resolve().parent
if str(TOOLS_DIR) not in sys.path:
    sys.path.insert(0, str(TOOLS_DIR))

from tx_observe_host import (
    HOST_CATALOG_PATH,
    LIVE_RAW_RECORD_HEADER_BYTES,
    ANALYZER_DECODER_VERSION,
    DS_METHOD_TRACK_ID,
    DerivedCacheResult,
    DerivedTables,
    LOCK_TRACK_ID,
    PARQUET_MANIFEST,
    ProjectionInput,
    analyze,
    analyze_allocation_tracks,
    analyze_clone_thread_phases,
    analyze_counter_phase_sequence,
    analyze_ds_method_metrics,
    analyze_futex_ops,
    analyze_futex_source_correlation,
    analyze_futex_table_counters,
    analyze_futex_wake_latency,
    analyze_lock_metrics,
    analyze_lock_service_counters,
    analyze_parquet_summary,
    analyze_projection,
    analyze_roundtrip,
    analyze_sched_counters,
    analyze_vm_poll_attribution,
    analyze_wait_source_notify,
    analyze_wake_hint_counters,
    allocation_rows,
    counter_rows,
    decode_task_code,
    decode_task_duration_us,
    fmt_counter_value,
    percentile,
    roundtrip_points,
    build_derived_tables,
    derived_cache_path,
    derived_tables_from_json,
    derived_tables_to_json,
    describe,
    event_name,
    export_derived_tables_parquet,
    export_projection_parquet,
    file_sha256,
    fmt_ns,
    fnv1a32,
    load_derived_tables_cache,
    load_or_build_derived_tables,
    parquet_manifest_matches,
    parquet_manifest_path,
    require_duckdb,
    run_python_file,
    run_python_file_with_table_dir,
    run_python_projection,
    run_sql_projection,
    run_sql_query,
    span_display_name,
    sql_names_rows,
    sql_records_rows,
    sql_repairs_rows,
    write_jsonl,
    RING_HEADER_SIZE,
    RING_LOST_OFF,
    RING_PRODUCER_OFF,
    SUPPORTED_HEADER_VERSION,
    TX_TRACE_MAGIC,
    TraceEventStream,
    TraceIntegrity,
    TraceLoadResult,
    TraceStreamMarker,
    duckdb_json_columns,
    duckdb_sql_string,
    load_rawrecords,
    load_rawrecords_stream,
    load_host_catalog,
    load_records,
    load_records_stream,
    load_txtrace_records,
    load_txtrace_stream,
    parquet_schema_catalog,
    parquet_select_sql,
    projection_schema_catalog,
    projection_schemas_from_host_catalog,
    read_json_typed,
    typed_select_sql,
    validate_host_catalog,
    validate_host_catalog_topology,
)
from tx_observe_host.l5_canonical import (
    KIND_NAMES,
    LEVEL_NAMES,
    MAX_HARTS_DAEMON,
    PAYLOAD_AGENT_STATE_CHANGE,
    PAYLOAD_ARG_VALUE,
    PAYLOAD_CLOCK_SNAPSHOT,
    PAYLOAD_COUNTER_VALUE,
    PAYLOAD_DRIVE_BEGIN,
    PAYLOAD_DRIVE_END,
    PAYLOAD_MIN_LENGTHS,
    PAYLOAD_MUTATION_INDEX_COMMIT,
    PAYLOAD_MUTATION_ZONE_SIGN,
    PAYLOAD_NONE,
    PAYLOAD_PANIC,
    PAYLOAD_PHASE_TRANSITION,
    PAYLOAD_PROCESS_FORK,
    PAYLOAD_PROCESS_GROUP,
    PAYLOAD_PROCESS_LABEL,
    PAYLOAD_RESUME,
    PAYLOAD_SCHED_SWITCH,
    PAYLOAD_STEP_OUTCOME,
    PAYLOAD_STRING_DESCRIPTOR,
    PAYLOAD_SYSCALL_ENTER,
    PAYLOAD_SYSCALL_EXIT,
    PAYLOAD_TRACK_DESCRIPTOR,
    PAYLOAD_WAIT_SOURCE_NOTIFY,
    PAYLOAD_YIELD_BEGIN,
    RECORD_MAGIC,
    RECORD_STRUCT,
    SUPPORTED_RECORD_SIZE,
    SUPPORTED_RECORD_VERSION,
    decode_payload_bytes,
    decode_record_bytes,
    event_order_key,
    is_repair_record,
    is_trace_record,
    parse_u32_id,
    parse_u64_id,
    payload_u64,
    record_hart,
    record_order_key,
    record_ts,
    repair_record,
    timeline_records,
    trace_stream_markers,
)

DEBUG_COUNTER_NAMES = [
    "debug.vm.pmap.teardown.phase",
    "debug.vm.pmap.teardown.pages",
    "debug.vm.pmap.drop.begin",
    "debug.vm.pmap.drop.mapped_pages",
    "debug.vm.pmap.drop.destroy_root",
    "debug.vm.pmap.drop.release_pins",
    "debug.vm.pmap.drop.end",
    "debug.vm.mmap.enter",
    "debug.vm.mmap.target",
    "debug.vm.mmap.pages",
    "debug.vm.mmap.range_start",
    "debug.vm.mmap.placement",
    "debug.vm.mmap.reserved",
    "debug.vm.mmap.committed_pages",
    "debug.vm.mmap.commit.phase",
    "debug.vm.mmap.commit.changed_pages",
    "debug.vm.mmap.commit.fixed_teardown",
    "debug.vm.map_path.mmap.anywhere_search_ns",
    "debug.vm.map_path.mmap.reserve_map_ns",
    "debug.vm.map_path.mmap.reserve_blocked_ns",
    "debug.vm.map_path.mmap.reserve_error_ns",
    "debug.vm.map_path.mmap.commit_ns",
    "debug.vm.map_path.mmap.commit_recipe_ns",
    "debug.vm.map_path.mmap.commit_fixed_pmap_teardown_ns",
    "debug.vm.map_path.mmap.commit_stats_ns",
    "debug.vm.map_path.mmap.changed_pages",
    "debug.vm.map_path.mmap.total_ns",
    "debug.vm.map_path.munmap.acquire_ns",
    "debug.vm.map_path.munmap.recipe_ns",
    "debug.vm.map_path.munmap.pmap_teardown_ns",
    "debug.vm.map_path.munmap.stats_ns",
    "debug.vm.map_path.munmap.changed_pages",
    "debug.vm.map_path.munmap.pmap_removed",
    "debug.vm.map_path.munmap.total_ns",
    "debug.vm.map_path.pmap.teardown_drain_ns",
    "debug.vm.map_path.pmap.teardown_removed_pages",
    "debug.vm.map_path.pmap.teardown_shifted_entries",
    "debug.vm.map_path.pmap.teardown_hal_unmap_ns",
    "debug.vm.map_path.pmap.teardown_loop_ns",
    "debug.vm.map_path.pmap.teardown_shootdown_ns",
    "debug.vm.map_path.pmap.teardown_total_ns",
    "debug.vm.unmap.phase",
    "debug.vm.unmap.changed_pages",
    "debug.vm.unmap.pmap_removed",
    "debug.vm.protect.phase",
    "debug.vm.protect.changed_pages",
    "debug.vm.protect.pmap_removed",
    "debug.vm.recipe.mmap",
    "debug.vm.recipe.mprotect",
    "debug.vm.recipe.count",
    "debug.vm.recipe.vm_size",
    "debug.vm.recipe.protect.phase",
    "debug.vm.recipe.protect.changed_pages",
    "debug.vm.recipe.protect.touched_entries",
    "debug.vm.recipe.rewrite_protect.phase",
    "debug.vm.recipe.rewrite_protect.overlap_count",
    "debug.vm.recipe.publish.touched_entries",
    "debug.vm.recipe.publish.retire_old",
    "debug.vm.recipe.phase.lock_wait_ns",
    "debug.vm.recipe.phase.rewrite_ns",
    "debug.vm.recipe.phase.publish_swap_ns",
    "debug.vm.recipe.phase.debug_emit_ns",
    "debug.vm.recipe.phase.retire_enqueue_ns",
    "debug.vm.recipe.phase.retire_queued",
    "debug.vm.recipe.phase.retire_error",
    "debug.vm.recipe.phase.deferred_enqueue_ns",
    "debug.vm.recipe.phase.deferred_enqueued",
    "debug.vm.recipe.phase.inline_fallback",
    "debug.vm.recipe.phase.deferred_drain_ns",
    "debug.vm.recipe.phase.deferred_drained",
    "debug.vm.recipe.phase.reclaim_drop_ns",
    "debug.clone_path.sys_clone.parent_ctx_ns",
    "debug.clone_path.sys_clone.step_thread_ns",
    "debug.clone_path.sys_clone.parent_settid_ns",
    "debug.clone_path.sys_clone.reactor_submit_ns",
    "debug.clone_path.sys_clone.clone_thread_count",
    "debug.clone_path.sys_clone.total_ns",
    "debug.clone_path.step_clone_thread.allocate_tid_ns",
    "debug.clone_path.step_clone_thread.sign_thread_ns",
    "debug.clone_path.step_clone_thread.register_tid_ns",
    "debug.clone_path.step_clone_thread.seed_context_ns",
    "debug.clone_path.step_clone_thread.clear_ctid_ns",
    "debug.clone_path.step_clone_thread.attach_ns",
    "debug.clone_path.step_clone_thread.count",
    "debug.clone_path.step_clone_thread.total_ns",
    "debug.clone_path.sign_thread.payload_fresh_ns",
    "debug.clone_path.sign_thread.payload_sign_ns",
    "debug.clone_path.sign_thread.payload_cap_ns",
    "debug.clone_path.sign_thread.identity_sign_ns",
    "debug.clone_path.sign_thread.count",
    "debug.clone_path.sign_thread.total_ns",
    "debug.clone_path.child_submit.payload_lookup_ns",
    "debug.clone_path.child_submit.payload_clone_ns",
    "debug.clone_path.child_submit.reactor_with_ns",
    "debug.clone_path.child_submit.terminal_drain_ns",
    "debug.clone_path.child_submit.register_task_ns",
    "debug.clone_path.child_submit.count",
    "debug.clone_path.child_submit.not_submitted",
    "debug.clone_path.child_submit.total_ns",
    "debug.vm.recipe.reclaim_tree.begin",
    "debug.vm.recipe.reclaim_tree.end",
    "debug.epoch.retire.enter",
    "debug.epoch.retire.reclaim_fn",
    "debug.epoch.retire.null",
    "debug.epoch.retire.not_initialized",
    "debug.epoch.retire.alloc_miss",
    "debug.epoch.retire.queued",
    "debug.epoch.retire.local_count",
    "debug.epoch.retire.threshold",
    "debug.epoch.drain.enter",
    "debug.epoch.drain.budget",
    "debug.epoch.drain.cpu",
    "debug.epoch.drain.safe_epoch",
    "debug.epoch.drain.exit",
    "debug.epoch.drain.advanced",
    "debug.epoch.drain.reclaimed",
    "debug.epoch.drain.remaining",
    "debug.epoch.drain.active_guards",
    "debug.epoch.reclaim.begin",
    "debug.epoch.reclaim.fn",
    "debug.epoch.reclaim.end",
    "debug.zone.reclaim_slot.begin",
    "debug.zone.reclaim_slot.kind",
    "debug.zone.reclaim_slot.size",
    "debug.zone.reclaim_slot.end",
    "debug.alloc.zone.reserve",
    "debug.alloc.zone.reserve.bucket_hit",
    "debug.alloc.zone.reserve.bucket_miss",
    "debug.alloc.zone.reserve.bucket_len",
    "debug.alloc.zone.reserve.bucket_len_after",
    "debug.alloc.zone.reserve.cpu",
    "debug.alloc.zone.reserve.refill_slots",
    "debug.alloc.zone.reserve.duration_ns",
    "debug.alloc.zone.reserve.refill.duration_ns",
    "debug.alloc.zone.bucket_refill",
    "debug.alloc.zone.bucket_refill.slots",
    "debug.alloc.zone.bucket_refill.len_before",
    "debug.alloc.zone.bucket_refill.len_after",
    "debug.alloc.zone.bucket_refill.duration_ns",
    "debug.alloc.zone.bucket_drain.slots",
    "debug.alloc.zone.bucket_drain.duration_ns",
    "debug.alloc.zone.keg.source.partial",
    "debug.alloc.zone.keg.source.empty",
    "debug.alloc.zone.keg.new_slab",
    "debug.alloc.zone.keg.new_slab.duration_ns",
    "debug.alloc.zone.keg.claim",
    "debug.alloc.zone.keg.claim.free_before",
    "debug.alloc.zone.keg.claim.from_new_slab",
    "debug.alloc.zone.keg.claim.duration_ns",
    "debug.alloc.zone.keg.pop.duration_ns",
    "debug.alloc.zone.keg.return",
    "debug.alloc.zone.keg.return.free_before",
    "debug.alloc.zone.keg.return.retire_candidate",
    "debug.alloc.zone.keg.return.retired_slab",
    "debug.alloc.zone.keg.return.retire_failed",
    "debug.alloc.zone.keg.return.retire.duration_ns",
    "debug.alloc.zone.keg.return.duration_ns",
    "debug.alloc.zone.slab.bitmap_scan",
    "debug.lock.wait_ns",
    "debug.lock.service_ns",
    "debug.lock.response_ns",
    "debug.lock.spins",
    "debug.lock.contended",
    "debug.signal.select.thread1.lock.request",
    "debug.signal.select.thread1.lock.acquired",
    "debug.signal.select.thread1.lock.release",
    "debug.signal.select.owner.upgrade.request",
    "debug.signal.select.owner.upgrade.done",
    "debug.signal.select.owner.upgrade.miss",
    "debug.signal.select.proc.lock.request",
    "debug.signal.select.proc.lock.acquired",
    "debug.signal.select.proc.lock.release",
    "debug.signal.select.thread2.lock.request",
    "debug.signal.select.thread2.lock.acquired",
    "debug.signal.select.thread2.lock.release",
    "debug.signal.select.thread_pending.hit",
    "debug.signal.select.group_pending.hit",
    "debug.signal.select.done",
    "debug.lock_service.process.payload.exit_group.shm_detach.duration_ns",
    "debug.lock_service.process.payload.exit_group.drain_fds.duration_ns",
    "debug.lock_service.process.payload.exit_group.threads_drain.duration_ns",
    "debug.lock_service.process.payload.exit_group.zombify_threads.duration_ns",
    "debug.lock_service.process.payload.exit_group.drop_drained.duration_ns",
    "debug.lock_service.process.payload.exit_group.payload_drop.duration_ns",
    "debug.lock_service.process.payload.process_exit.shm_detach.duration_ns",
    "debug.lock_service.process.payload.process_exit.drain_fds.duration_ns",
    "debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns",
    "debug.lock_service.process.payload.process_exit.payload_drop.duration_ns",
    "debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns",
    "debug.lock_service.process.payload.thread_exit.thread_count.duration_ns",
    "debug.lock_service.process.payload.thread_exit.group_exit.duration_ns",
    "debug.lock_service.process.payload.robust.head_reads.duration_ns",
    "debug.lock_service.process.payload.robust.entries.duration_ns",
    "debug.lock_service.process.payload.robust.pending.duration_ns",
    "debug.lock_service.process.payload.robust.entry_count",
    "debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns",
    "debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns",
    "debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns",
    "debug.lock_service.thread.payload.sigprocmask.payload_missing",
    "debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns",
    "debug.lock_service.thread.payload.sigprocmask.mask_noop",
    "debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns",
    "debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns",
    "debug.cap.upgrade.to_cap.duration_ns",
    "debug.cap.upgrade.to_cap.attempts",
    "debug.cap.upgrade.to_cap.retries",
    "debug.sigprocmask.enter",
    "debug.sigprocmask.args",
    "debug.sigprocmask.bad_size",
    "debug.sigprocmask.read.err",
    "debug.sigprocmask.read.after",
    "debug.sigprocmask.step.after",
    "debug.sigprocmask.step.zombie",
    "debug.sigprocmask.mask.zombie",
    "debug.sigprocmask.mask.after",
    "debug.sigprocmask.write.err",
    "debug.sigprocmask.write.after",
    "debug.sigprocmask.return",
    "debug.vm.user.copy_in.len",
    "debug.vm.user.copy_in.phase",
    "debug.vm.user.copy_in.err",
    "debug.vm.user.copy_in.chunk",
    "debug.vm.user.copy_in.copied",
    "debug.vm.user.copy_in.blocked",
    "debug.vm.user.resolve.kind",
    "debug.vm.user.resolve.phase",
    "debug.vm.user.resolve.err",
    "debug.vm.user.resolve.blocked",
    "debug.vm.user.resolve.backing",
    "debug.vm.user.resolve_page.backing",
    "debug.vm.user.resolve_page.err",
    "debug.vm.user.resolve_page.phase",
    "debug.vm.user.pagebacked.phase",
    "debug.vm.user.pagebacked.err",
    "debug.vm.user.pagebacked.blocked",
]

def harvest_source_debug_names(root: Path) -> dict[int, str]:
    """Best-effort decode table for in-tree `debug.*` observe counters."""
    names: dict[int, str] = {}
    for crate_dir in (
        "crates/tx-kernel",
        "crates/tx-shims",
        "crates/tx-subsystems",
        "crates/tx-reactor",
        "crates/tx-substrate",
    ):
        base = root / crate_dir
        if not base.exists():
            continue
        for source in base.rglob("*.rs"):
            try:
                text = source.read_text()
            except UnicodeDecodeError:
                continue
            for match in re.finditer(r'(?:b)?"(debug\.[^"]+)"', text):
                name = match.group(1)
                names[fnv1a32(name)] = name
    return names


def load_names(path: Path | None, source_root: Path | None = None) -> dict[int, str]:
    out: dict[int, str] = {fnv1a32(name): name for name in DEBUG_COUNTER_NAMES}
    if source_root is not None:
        out.update(harvest_source_debug_names(source_root))
    if path is None or not path.exists():
        return out
    data = json.loads(path.read_text())
    table = data.get("name_table", {})
    for key, value in table.items():
        try:
            out[int(key)] = str(value)
        except ValueError:
            continue
    return out


def input_arg_path(args: argparse.Namespace) -> Path:
    if args.ndjson is not None:
        return args.ndjson
    if args.rawrecords is not None:
        return args.rawrecords
    return args.file


def default_parquet_dir(input_path: Path) -> Path:
    return input_path.with_name(input_path.name + ".parquet")


def load_input_records(args: argparse.Namespace) -> list[dict[str, Any]]:
    return load_input_stream(args).records


def load_input_stream(args: argparse.Namespace) -> TraceEventStream:
    if args.ndjson is not None:
        return load_records_stream(args.ndjson, sort_records=not args.no_sort)
    if args.rawrecords is not None:
        return load_rawrecords_stream(args.rawrecords, sort_records=not args.no_sort)
    return load_txtrace_stream(args.file, sort_records=not args.no_sort)


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    inputs = parser.add_mutually_exclusive_group(required=True)
    inputs.add_argument("--ndjson", type=Path)
    inputs.add_argument("--file", type=Path, help="Read a txtrace-v0 binary trace directly")
    inputs.add_argument("--rawrecords", type=Path, help="Read a live-drain trace.rawrecords file directly")
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument("--sql", help="Run an inline DuckDB SQL query over analyzer views")
    actions.add_argument("--sql-file", type=Path, help="Run a DuckDB SQL query file over analyzer views")
    actions.add_argument("--python-file", type=Path, help="Run a Python script with derived Parquet table env vars")
    parser.add_argument("--names", type=Path)
    parser.add_argument("--top", type=int, default=20)
    parser.add_argument("--roundtrip-sysno", type=int, default=173)
    parser.add_argument(
        "--sql-format",
        choices=("table", "csv", "json"),
        default="table",
        help="Output format for --sql or --sql-file.",
    )
    parser.add_argument(
        "--dangling-spans",
        choices=("exclude", "synthetic-end"),
        default="exclude",
        help=(
            "Policy for SpanBegin records without a matching SpanEnd. The "
            "default excludes them from duration rankings; synthetic-end "
            "materializes them at the last retained record timestamp."
        ),
    )
    parser.add_argument(
        "--cache-dir",
        type=Path,
        help=(
            "Directory for derived span/counter/allocation tables. Cache keys "
            "include sha256(input) and the analyzer decoder version."
        ),
    )
    parser.add_argument(
        "--parquet-dir",
        type=Path,
        help=(
            "Optional directory for exported derived-table Parquet files "
            "(spans.parquet, counters.parquet, allocation_rows.parquet, "
            "sched_intervals.parquet, lock_rows.parquet, ds_method_rows.parquet)."
        ),
    )
    parser.add_argument(
        "--no-sort",
        action="store_true",
        help=(
            "Keep replay order instead of sorting by timestamp. This is useful "
            "for very large live-drain traces where approximate wall-level "
            "attribution is more important than exact cross-hart ordering."
        ),
    )
    parser.add_argument(
        "--source-root",
        type=Path,
        default=Path.cwd(),
        help=(
            "Repository root used to decode in-tree debug.* observe counter "
            "names. Pass /dev/null to disable source scanning."
        ),
    )
    return parser


def main() -> int:
    parser = build_arg_parser()
    args = parser.parse_args()

    if HOST_CATALOG_PATH.exists():
        validate_host_catalog(HOST_CATALOG_PATH)

    input_path = input_arg_path(args)
    if (
        args.parquet_dir is None
        and args.sql is None
        and args.sql_file is None
        and args.python_file is None
    ):
        args.parquet_dir = default_parquet_dir(input_path)
    source_root = None if str(args.source_root) == "/dev/null" else args.source_root
    names = load_names(args.names, source_root)
    if (
        args.parquet_dir is not None
        and args.sql is None
        and args.sql_file is None
        and args.python_file is None
        and args.cache_dir is not None
    ):
        input_hash = file_sha256(input_path)
        if parquet_manifest_matches(
            args.parquet_dir,
            input_hash=input_hash,
            dangling_span_policy=args.dangling_spans,
        ):
            print(f"parquet cache: hit {parquet_manifest_path(args.parquet_dir)}")
            print(analyze_parquet_summary(args.parquet_dir, names, args.top))
            return 0
        cached = load_derived_tables_cache(
            input_path,
            cache_dir=args.cache_dir,
            dangling_span_policy=args.dangling_spans,
        )
        if cached is not None:
            print(f"derived cache: hit {cached.path}")
            written = export_derived_tables_parquet(
                cached.tables,
                args.parquet_dir,
                input_hash=input_hash,
                dangling_span_policy=args.dangling_spans,
            )
            print("parquet export: " + " ".join(str(path) for path in written))
            print(analyze_parquet_summary(args.parquet_dir, names, args.top))
            return 0

    stream = load_input_stream(args)
    records = stream.records
    derived_result = load_or_build_derived_tables(
        input_path,
        records,
        cache_dir=args.cache_dir,
        dangling_span_policy=args.dangling_spans,
    )
    if derived_result.path is not None:
        state = "hit" if derived_result.hit else "miss"
        print(f"derived cache: {state} {derived_result.path}")
    projection = ProjectionInput(
        stream,
        names,
        derived_result.tables,
    )
    if args.sql is not None or args.sql_file is not None:
        query = args.sql if args.sql is not None else args.sql_file.read_text()
        print(run_sql_projection(projection, query, args.sql_format), end="")
        return 0
    if args.python_file is not None:
        proc = run_python_projection(
            args.python_file,
            projection,
            input_path,
            args.names,
            args.parquet_dir,
        )
        if proc.stdout:
            print(proc.stdout, end="")
        if proc.stderr:
            print(proc.stderr, end="", file=sys.stderr)
        return proc.returncode
    if args.parquet_dir is not None:
        written = export_projection_parquet(
            projection,
            args.parquet_dir,
            input_hash=file_sha256(input_path),
            dangling_span_policy=args.dangling_spans,
        )
        print("parquet export: " + " ".join(str(path) for path in written))
        print(analyze_parquet_summary(args.parquet_dir, names, args.top))
        return 0
    print(analyze(records, names, args.top, derived_result.tables))
    print(analyze_allocation_tracks(records, names, args.top, derived_result.tables))
    print(analyze_lock_metrics(records, names, args.top, derived_result.tables))
    print(analyze_lock_service_counters(records, names, args.top, derived_result.tables))
    print(analyze_ds_method_metrics(records, names, args.top, derived_result.tables))
    print(analyze_futex_ops(records, names, args.top))
    print(analyze_futex_table_counters(records, names, args.top))
    print(analyze_wait_source_notify(records, names, args.top))
    print(analyze_futex_source_correlation(records, names, args.top))
    print(analyze_futex_wake_latency(records, names, args.top))
    print(analyze_sched_counters(records, names, args.top))
    print(analyze_wake_hint_counters(records, names, args.top))
    print(analyze_vm_poll_attribution(records, names, args.top))
    print(analyze_roundtrip(records, names, args.roundtrip_sysno, args.top))
    print(analyze_clone_thread_phases(records, names, args.top))
    print(
        analyze_counter_phase_sequence(
            "child_submit",
            records,
            names,
            CHILD_SUBMIT_PHASES,
            args.top,
            key_by_value=True,
        )
    )
    print(
        analyze_counter_phase_sequence(
            "task_submit",
            records,
            names,
            TASK_SUBMIT_PHASES,
            args.top,
            key_by_value=False,
        )
    )
    print(
        analyze_counter_phase_sequence(
            "thread_exit",
            records,
            names,
            THREAD_EXIT_PHASES,
            args.top,
            key_by_value=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
