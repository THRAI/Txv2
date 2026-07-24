"""L6 derived table, SQL, Parquet, and Python-hook projections."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path
from typing import Any

from ..l5_canonical import (
    PAYLOAD_ARG_VALUE,
    PAYLOAD_SCHED_SWITCH,
    is_trace_record,
    parse_u32_id,
    parse_u64_id,
    payload_u64,
    record_hart,
    record_ts,
    timeline_records,
)
from . import (
    ProjectionInput,
    duckdb_sql_string,
    parquet_schema_catalog,
    parquet_select_sql,
    projection_schema_catalog,
    typed_select_sql,
)

ANALYZER_DECODER_VERSION = "tx-observe-analyze-derived-v5"
PARQUET_MANIFEST = "_tx_observe_parquet.json"
LOCK_TRACK_ID = 0xD500_0000_0000_000F
DS_METHOD_TRACK_ID = 0xD500_0000_0000_0010


ALLOC_TRACK_NAMES = {
    0xD500_0000_0000_0001: "debug.alloc.zone.slab",
    0xD500_0000_0000_0002: "debug.alloc.page_frame",
    0xD500_0000_0000_0003: "debug.alloc.page_run",
    0xD500_0000_0000_0004: "debug.alloc.vm.recipe_node",
    0xD500_0000_0000_0005: "debug.alloc.vm.private_page_node",
    0xD500_0000_0000_0006: "debug.alloc.pagebacked.cache",
    0xD500_0000_0000_0007: "debug.alloc.vm.address_space",
    0xD500_0000_0000_0008: "debug.alloc.pagebacked.container",
    0xD500_0000_0000_0009: "debug.alloc.thread.payload",
    0xD500_0000_0000_000A: "debug.alloc.thread.identity",
    0xD500_0000_0000_000B: "debug.alloc.process.payload",
    0xD500_0000_0000_000C: "debug.alloc.process.identity",
    0xD500_0000_0000_000D: "debug.alloc.process.threads",
    0xD500_0000_0000_000E: "debug.alloc.pidns",
}

def fnv1a32(text: str) -> int:
    value = 0x811C9DC5
    for byte in text.encode():
        value ^= byte
        value = (value * 0x01000193) & 0xFFFFFFFF
    return value


def fmt_ns(ns: float) -> str:
    return f"{ns / 1000.0:.1f}us"


def event_name(record: dict[str, Any], names: dict[int, str]) -> str:
    payload = record.get("payload") or {}
    sysno = payload.get("sysno")
    if isinstance(sysno, int):
        return names.get(sysno, f"sys_{sysno}")
    name_id = parse_u32_id(record.get("name_id"))
    if name_id is not None:
        return names.get(name_id, f"name_0x{name_id:x}")
    return "unknown"


class DerivedTables:
    def __init__(
        self,
        spans: list[dict[str, Any]],
        counters: list[dict[str, int]],
        allocation_rows: list[dict[str, Any]],
        meta: dict[str, Any],
        sched_intervals: list[dict[str, int]] | None = None,
        lock_rows: list[dict[str, int]] | None = None,
        ds_method_rows: list[dict[str, int]] | None = None,
    ) -> None:
        self.spans = spans
        self.counters = counters
        self.allocation_rows = allocation_rows
        self.sched_intervals = sched_intervals or []
        self.lock_rows = lock_rows or []
        self.ds_method_rows = ds_method_rows or []
        self.meta = meta


class DerivedCacheResult:
    def __init__(
        self,
        tables: DerivedTables,
        path: Path | None = None,
        hit: bool = False,
    ) -> None:
        self.tables = tables
        self.path = path
        self.hit = hit


def span_display_name(span: dict[str, Any], names: dict[int, str]) -> str:
    name = span.get("name")
    if isinstance(name, str):
        return name
    sysno = span.get("sysno")
    if isinstance(sysno, int):
        return names.get(sysno, f"sys_{sysno}")
    name_id = span.get("name_id")
    if isinstance(name_id, int):
        return names.get(name_id, f"name_0x{name_id:x}")
    return "unknown"


def describe(record: dict[str, Any], spans: dict[str, dict[str, Any]], names: dict[int, str]) -> str:
    kind = record.get("kind")
    span = record.get("span")
    if kind == "SpanEnd" and span in spans:
        info = spans[span]
        return f"{kind} {span} {span_display_name(info, names)}"
    if kind == "SpanBegin":
        return f"{kind} {span} {event_name(record, names)}"
    if kind == "Counter":
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        cname = names.get(cid, str(cid)) if isinstance(cid, int) else str(cid)
        return f"Counter {cname}={payload.get('value')}"
    return f"{kind} name={record.get('name_id')} span={span} parent={record.get('parent')}"


def build_derived_tables(
    records: list[dict[str, Any]],
    *,
    dangling_span_policy: str = "exclude",
) -> DerivedTables:
    begins: dict[str, dict[str, Any]] = {}
    span_tasks: dict[str, tuple[int | None, int | None]] = {}
    sched_begins: dict[str, dict[str, Any]] = {}
    current_task_by_hart: dict[int, int] = {}
    current_pid_by_hart: dict[int, int] = {}
    sched_intervals: list[dict[str, int]] = []
    spans: list[dict[str, Any]] = []
    counters: list[dict[str, int]] = []
    allocation: list[dict[str, Any]] = []
    lock_rows: list[dict[str, int]] = []
    ds_method_rows: list[dict[str, int]] = []
    pending_ds_zone_by_emit: dict[tuple[int, int], int] = {}
    ds_zone_metric_id = fnv1a32("debug.ds.method.zone_id")

    for record in records:
        kind = record.get("kind")
        span_id = record.get("span")
        payload = record.get("payload") or {}
        hart = record_hart(record)
        if record.get("payload_tag") == PAYLOAD_SCHED_SWITCH:
            task = payload_u64(payload, "task_id_low")
            pid = payload_u64(payload, "process_id_low")
            sched_hart = payload_u64(payload, "hart_id")
            event_hart = int(sched_hart if sched_hart is not None else hart or 0)
            if kind == "SpanBegin" and isinstance(span_id, str) and task is not None:
                sched_begins[span_id] = record
                current_task_by_hart[event_hart] = task
                current_pid_by_hart[event_hart] = int(pid or 0)
            elif kind == "SpanEnd" and isinstance(span_id, str):
                begin = sched_begins.pop(span_id, None)
                begin_payload = (begin or {}).get("payload") or {}
                begin_task = payload_u64(begin_payload, "task_id_low")
                begin_pid = payload_u64(begin_payload, "process_id_low")
                begin_hart = payload_u64(begin_payload, "hart_id")
                if begin is not None and begin_task is not None:
                    sched_intervals.append(
                        {
                            "task": begin_task,
                            "pid": int(begin_pid or 0),
                            "hart": int(begin_hart if begin_hart is not None else record_hart(begin) or event_hart),
                            "begin": record_ts(begin),
                            "end": record_ts(record),
                            "dur": record_ts(record) - record_ts(begin),
                        }
                    )
                current_task_by_hart.pop(event_hart, None)
                current_pid_by_hart.pop(event_hart, None)

        if kind == "SpanBegin" and isinstance(span_id, str):
            begins[span_id] = record
            if record.get("payload_tag") != PAYLOAD_SCHED_SWITCH and hart in current_task_by_hart:
                span_tasks[span_id] = (
                    current_task_by_hart.get(hart),
                    current_pid_by_hart.get(hart),
                )
        elif kind == "SpanEnd" and isinstance(span_id, str):
            begin = begins.pop(span_id, None)
            if begin is None:
                continue
            payload = begin.get("payload") or {}
            sysno = payload.get("sysno")
            task_id, process_id = span_tasks.pop(span_id, (None, None))
            spans.append(
                {
                    "span": span_id,
                    "name_id": parse_u32_id(begin.get("name_id")),
                    "sysno": sysno,
                    "begin": record_ts(begin),
                    "end": record_ts(record),
                    "dur": record_ts(record) - record_ts(begin),
                    "begin_hart": record_hart(begin),
                    "end_hart": record_hart(record),
                    "task_id": task_id,
                    "process_id": process_id,
                    "ret": (record.get("payload") or {}).get("ret"),
                    "errno": (record.get("payload") or {}).get("errno"),
                    "dangling": False,
                }
            )
        elif kind == "Counter":
            payload = record.get("payload") or {}
            cid = payload.get("counter_id")
            value = payload_u64(payload, "value")
            if isinstance(cid, int) and value is not None:
                counters.append({"ts": record_ts(record), "counter_id": cid, "value": value})
        elif kind == "Instant" and record.get("payload_tag") == PAYLOAD_ARG_VALUE:
            parent = parse_u64_id(record.get("parent"))
            payload = record.get("payload") or {}
            value = payload_u64(payload, "value0")
            if value is None:
                continue
            if parent == LOCK_TRACK_ID:
                lock_id = parse_u32_id(record.get("name_id"))
                metric_id = parse_u32_id(payload.get("key"))
                if lock_id is None or metric_id is None:
                    continue
                lock_rows.append(
                    {
                        "ts": record_ts(record),
                        "hart": int(hart or 0),
                        "lock_id": lock_id,
                        "metric_id": metric_id,
                        "value": value,
                    }
                )
                continue
            if parent == DS_METHOD_TRACK_ID:
                method_id = parse_u32_id(record.get("name_id"))
                metric_id = parse_u32_id(payload.get("key"))
                if method_id is None or metric_id is None:
                    continue
                emit_key = (int(hart or 0), method_id)
                if metric_id == ds_zone_metric_id:
                    pending_ds_zone_by_emit[emit_key] = int(value)
                    continue
                zone_id = pending_ds_zone_by_emit.pop(emit_key, None)
                ds_method_rows.append(
                    {
                        "ts": record_ts(record),
                        "hart": emit_key[0],
                        "method_id": method_id,
                        "zone_id": zone_id or None,
                        "metric_id": metric_id,
                        "value": value,
                    }
                )
                continue
            if parent not in ALLOC_TRACK_NAMES:
                continue
            allocation.append(
                {
                    "ts": record_ts(record),
                    "track": ALLOC_TRACK_NAMES[parent],
                    "name_id": parse_u32_id(record.get("name_id")),
                    "value": value,
                }
            )

    dangling_count = len(begins)
    trace_tail = timeline_records(records)
    if dangling_span_policy == "synthetic-end" and trace_tail:
        last_ts = record_ts(trace_tail[-1])
        last_hart = record_hart(trace_tail[-1])
        for span_id, begin in begins.items():
            payload = begin.get("payload") or {}
            task_id, process_id = span_tasks.get(span_id, (None, None))
            spans.append(
                {
                    "span": span_id,
                    "name_id": parse_u32_id(begin.get("name_id")),
                    "sysno": payload.get("sysno"),
                    "begin": record_ts(begin),
                    "end": last_ts,
                    "dur": max(0, last_ts - record_ts(begin)),
                    "begin_hart": record_hart(begin),
                    "end_hart": last_hart,
                    "task_id": task_id,
                    "process_id": process_id,
                    "ret": None,
                    "errno": None,
                    "dangling": True,
                }
            )

    annotate_span_sched_migration(spans, sched_intervals)

    return DerivedTables(
        spans=spans,
        counters=counters,
        allocation_rows=allocation,
        sched_intervals=sched_intervals,
        lock_rows=lock_rows,
        ds_method_rows=ds_method_rows,
        meta={
            "schema": "tx-observe-derived-v0",
            "decoder_version": ANALYZER_DECODER_VERSION,
            "record_count": len(records),
            "dangling_span_policy": dangling_span_policy,
            "dangling_span_count": dangling_count,
            "dangling_spans_materialized": dangling_count
            if dangling_span_policy == "synthetic-end" and records
            else 0,
            "span_count": len(spans),
            "counter_count": len(counters),
            "allocation_row_count": len(allocation),
            "sched_interval_count": len(sched_intervals),
            "lock_row_count": len(lock_rows),
            "ds_method_row_count": len(ds_method_rows),
        },
    )


def annotate_span_sched_migration(
    spans: list[dict[str, Any]],
    sched_intervals: list[dict[str, int]],
) -> None:
    intervals_by_task: dict[int, list[dict[str, int]]] = defaultdict(list)
    for interval in sched_intervals:
        intervals_by_task[interval["task"]].append(interval)
    for rows in intervals_by_task.values():
        rows.sort(key=lambda row: row["begin"])

    for span in spans:
        task_id = span.get("task_id")
        if not isinstance(task_id, int):
            continue
        begin = int(span["begin"])
        end = int(span["end"])
        harts: list[int] = []
        coverage = 0
        segments = 0
        for interval in intervals_by_task.get(task_id, []):
            if interval["end"] <= begin:
                continue
            if interval["begin"] >= end:
                break
            overlap = min(end, interval["end"]) - max(begin, interval["begin"])
            if overlap <= 0:
                continue
            segments += 1
            coverage += overlap
            hart = interval["hart"]
            if hart not in harts:
                harts.append(hart)
        if harts:
            span["sched_harts"] = harts
            span["sched_segments"] = segments
            span["sched_coverage"] = coverage
            span["sched_migrated"] = len(harts) > 1


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def derived_cache_path(cache_dir: Path, input_hash: str, dangling_span_policy: str) -> Path:
    name = f"{input_hash}.{ANALYZER_DECODER_VERSION}.{dangling_span_policy}.json"
    return cache_dir / name


def derived_tables_to_json(tables: DerivedTables, input_hash: str) -> dict[str, Any]:
    return {
        "meta": {**tables.meta, "input_hash": input_hash},
        "spans": tables.spans,
        "counters": tables.counters,
        "allocation_rows": tables.allocation_rows,
        "sched_intervals": tables.sched_intervals,
        "lock_rows": tables.lock_rows,
        "ds_method_rows": tables.ds_method_rows,
    }


def derived_tables_from_json(data: dict[str, Any]) -> DerivedTables:
    return DerivedTables(
        spans=list(data.get("spans") or []),
        counters=list(data.get("counters") or []),
        allocation_rows=list(data.get("allocation_rows") or []),
        sched_intervals=list(data.get("sched_intervals") or []),
        lock_rows=list(data.get("lock_rows") or []),
        ds_method_rows=list(data.get("ds_method_rows") or []),
        meta=dict(data.get("meta") or {}),
    )


def sql_records_rows(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for record in records:
        if not is_trace_record(record):
            continue
        rows.append(
            {
                "ts": record_ts(record),
                "hart": record_hart(record),
                "seq": parse_u64_id(record.get("seq")),
                "kind": record.get("kind"),
                "level": record.get("level"),
                "span": record.get("span"),
                "parent": record.get("parent"),
                "name_id": parse_u32_id(record.get("name_id")),
                "payload_tag": parse_u32_id(record.get("payload_tag")),
            }
        )
    return rows


def sql_repairs_rows(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for record in records:
        if record.get("kind") != "Repair":
            continue
        rows.append(
            {
                "hart": record_hart(record),
                "seq_around": parse_u64_id(record.get("seq_around")),
                "category": record.get("category"),
                "details": record.get("details"),
            }
        )
    return rows


def sql_names_rows(names: dict[int, str]) -> list[dict[str, Any]]:
    return [{"id": int(name_id), "name": name} for name_id, name in sorted(names.items())]


def write_jsonl(path: Path, rows: list[dict[str, Any]], columns: list[str]) -> None:
    with path.open("w", encoding="utf-8") as file:
        for row in rows:
            file.write(json.dumps({column: row.get(column) for column in columns}, separators=(",", ":")))
            file.write("\n")


def require_duckdb() -> str:
    duckdb = shutil.which("duckdb")
    if duckdb is None:
        raise RuntimeError("duckdb CLI is required for SQL/Parquet analysis")
    return duckdb


def parquet_manifest_path(out_dir: Path) -> Path:
    return out_dir / PARQUET_MANIFEST


def parquet_manifest_matches(
    out_dir: Path,
    *,
    input_hash: str,
    dangling_span_policy: str,
) -> bool:
    path = parquet_manifest_path(out_dir)
    if not path.exists():
        return False
    try:
        manifest = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return False
    if (
        manifest.get("schema") != "tx-observe-derived-parquet-v0"
        or manifest.get("input_hash") != input_hash
        or manifest.get("decoder_version") != ANALYZER_DECODER_VERSION
        or manifest.get("dangling_span_policy") != dangling_span_policy
    ):
        return False
    files = manifest.get("files") or []
    expected = set(parquet_schema_catalog())
    if set(files) != expected:
        return False
    return all((out_dir / filename).exists() for filename in expected)


def write_parquet_manifest(
    tables: DerivedTables,
    out_dir: Path,
    *,
    input_hash: str,
    dangling_span_policy: str,
) -> None:
    manifest = {
        "schema": "tx-observe-derived-parquet-v0",
        "input_hash": input_hash,
        "decoder_version": ANALYZER_DECODER_VERSION,
        "dangling_span_policy": dangling_span_policy,
        "files": sorted(parquet_schema_catalog()),
        "table_counts": {
            "spans": len(tables.spans),
            "counters": len(tables.counters),
            "allocation_rows": len(tables.allocation_rows),
            "sched_intervals": len(tables.sched_intervals),
            "lock_rows": len(tables.lock_rows),
            "ds_method_rows": len(tables.ds_method_rows),
        },
    }
    path = parquet_manifest_path(out_dir)
    tmp = path.with_suffix(path.suffix + f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    tmp.replace(path)


def export_derived_tables_parquet(
    tables: DerivedTables,
    out_dir: Path,
    *,
    input_hash: str | None = None,
    dangling_span_policy: str | None = None,
) -> list[Path]:
    duckdb = require_duckdb()
    out_dir.mkdir(parents=True, exist_ok=True)
    schemas = projection_schema_catalog()
    written: list[Path] = []
    exports = [
        (
            "spans.parquet",
            tables.spans,
            [column for column, _ in schemas["spans.parquet"]],
        ),
        ("counters.parquet", tables.counters, [column for column, _ in schemas["counters.parquet"]]),
        (
            "allocation_rows.parquet",
            tables.allocation_rows,
            [column for column, _ in schemas["allocation_rows.parquet"]],
        ),
        (
            "sched_intervals.parquet",
            tables.sched_intervals,
            [column for column, _ in schemas["sched_intervals.parquet"]],
        ),
        (
            "lock_rows.parquet",
            tables.lock_rows,
            [column for column, _ in schemas["lock_rows.parquet"]],
        ),
        (
            "ds_method_rows.parquet",
            tables.ds_method_rows,
            [column for column, _ in schemas["ds_method_rows.parquet"]],
        ),
    ]
    for filename, rows, columns in exports:
        json_path = out_dir / f"{filename}.jsonl"
        parquet_path = out_dir / filename
        write_jsonl(json_path, rows, columns)
        select_sql = parquet_select_sql(filename, json_path, rows)
        sql = f"COPY ({select_sql}) TO {duckdb_sql_string(parquet_path)} (FORMAT PARQUET);"
        try:
            proc = subprocess.run([duckdb, "--no-stdin", "-c", sql], capture_output=True, text=True)
            if proc.returncode != 0:
                raise RuntimeError(f"duckdb parquet export failed: {proc.stderr.strip() or proc.stdout.strip()}")
            written.append(parquet_path)
        finally:
            json_path.unlink(missing_ok=True)
    if input_hash is not None and dangling_span_policy is not None:
        write_parquet_manifest(
            tables,
            out_dir,
            input_hash=input_hash,
            dangling_span_policy=dangling_span_policy,
        )
    return written


def sql_format_args(fmt: str) -> list[str]:
    if fmt == "csv":
        return ["-csv"]
    if fmt == "json":
        return ["-json"]
    return []


def run_sql_query(
    records: list[dict[str, Any]],
    tables: DerivedTables,
    names: dict[int, str],
    query: str,
    sql_format: str,
) -> str:
    duckdb = require_duckdb()
    schemas = projection_schema_catalog()
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        table_defs = [
            ("records", sql_records_rows(records), schemas["records"]),
            ("repairs", sql_repairs_rows(records), schemas["repairs"]),
            ("spans", tables.spans, schemas["spans.parquet"]),
            ("counters", tables.counters, schemas["counters.parquet"]),
            ("allocation_rows", tables.allocation_rows, schemas["allocation_rows.parquet"]),
            ("sched_intervals", tables.sched_intervals, schemas["sched_intervals.parquet"]),
            ("lock_rows", tables.lock_rows, schemas["lock_rows.parquet"]),
            ("ds_method_rows", tables.ds_method_rows, schemas["ds_method_rows.parquet"]),
            ("names", sql_names_rows(names), schemas["names"]),
        ]
        setup: list[str] = []
        for table_name, rows, schema in table_defs:
            json_path = tmp / f"{table_name}.jsonl"
            columns = [column for column, _ in schema]
            write_jsonl(json_path, rows, columns)
            setup.append(
                f"CREATE TEMP VIEW {table_name} AS {typed_select_sql(json_path, rows, schema)};"
            )
        sql = "\n".join(setup + [query])
        proc = subprocess.run(
            [duckdb, "--no-stdin", *sql_format_args(sql_format), "-c", sql],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"duckdb SQL query failed: {proc.stderr.strip() or proc.stdout.strip()}")
        return proc.stdout


def run_sql_projection(projection: ProjectionInput, query: str, sql_format: str) -> str:
    tables = projection.derived or build_derived_tables(projection.stream.records)
    return run_sql_query(projection.stream.records, tables, projection.names, query, sql_format)


def analyze_parquet_summary(parquet_dir: Path, names: dict[int, str], top: int) -> str:
    duckdb = require_duckdb()
    name_values = ", ".join(
        f"({int(name_id)}, {duckdb_sql_string(name)})"
        for name_id, name in sorted(names.items())
    )
    names_source = (
        f"(VALUES {name_values}) AS t(id, name)"
        if name_values
        else "(SELECT NULL::UINTEGER AS id, NULL::VARCHAR AS name WHERE false)"
    )
    sql = f"""
CREATE TEMP TABLE names AS
SELECT CAST(id AS UINTEGER) AS id, CAST(name AS VARCHAR) AS name
FROM {names_source};

CREATE TEMP VIEW spans AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "spans.parquet")});
CREATE TEMP VIEW counters AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "counters.parquet")});
CREATE TEMP VIEW allocation_rows AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "allocation_rows.parquet")});
CREATE TEMP VIEW sched_intervals AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "sched_intervals.parquet")});
CREATE TEMP VIEW lock_rows AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "lock_rows.parquet")});
CREATE TEMP VIEW ds_method_rows AS SELECT * FROM read_parquet({duckdb_sql_string(parquet_dir / "ds_method_rows.parquet")});

SELECT 'counts' AS section, 'spans' AS key, count(*)::VARCHAR AS n, NULL AS total_ns,
       NULL AS p50_ns, NULL AS p99_ns, NULL AS max_ns, 0::UBIGINT AS order_ns
FROM spans
UNION ALL
SELECT 'counts', 'counters', count(*)::VARCHAR, NULL, NULL, NULL, NULL, 0::UBIGINT FROM counters
UNION ALL
SELECT 'counts', 'allocation_rows', count(*)::VARCHAR, NULL, NULL, NULL, NULL, 0::UBIGINT FROM allocation_rows
UNION ALL
SELECT 'counts', 'sched_intervals', count(*)::VARCHAR, NULL, NULL, NULL, NULL, 0::UBIGINT FROM sched_intervals
UNION ALL
SELECT 'counts', 'lock_rows', count(*)::VARCHAR, NULL, NULL, NULL, NULL, 0::UBIGINT FROM lock_rows
UNION ALL
SELECT 'counts', 'ds_method_rows', count(*)::VARCHAR, NULL, NULL, NULL, NULL, 0::UBIGINT FROM ds_method_rows
UNION ALL
SELECT 'span_total',
       coalesce(names.name, 'name_0x' || lower(hex(s.name_id))) AS key,
       count(*)::VARCHAR AS n,
       sum(s.dur)::VARCHAR AS total_ns,
       quantile_disc(s.dur, 0.50)::VARCHAR AS p50_ns,
       quantile_disc(s.dur, 0.99)::VARCHAR AS p99_ns,
       max(s.dur)::VARCHAR AS max_ns,
       sum(s.dur)::UBIGINT AS order_ns
FROM spans s
LEFT JOIN names ON names.id = s.name_id
GROUP BY key
ORDER BY section, order_ns DESC
LIMIT {top + 4};
"""
    proc = subprocess.run(
        [duckdb, "--no-stdin", "-csv", "-c", sql],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"duckdb parquet summary failed: {proc.stderr.strip() or proc.stdout.strip()}")

    lines = [line for line in proc.stdout.splitlines() if line]
    if not lines:
        return "\nderived parquet summary: none"
    rows = [line.split(",") for line in lines[1:]]
    counts = {row[1]: int(row[2]) for row in rows if row[0] == "counts"}
    out = [
        "",
        "derived parquet summary:",
        (
            f"spans={counts.get('spans', 0)} counters={counts.get('counters', 0)} "
            f"allocation_rows={counts.get('allocation_rows', 0)} "
            f"sched_intervals={counts.get('sched_intervals', 0)} "
            f"lock_rows={counts.get('lock_rows', 0)} "
            f"ds_method_rows={counts.get('ds_method_rows', 0)}"
        ),
        "",
        "by span total (parquet):",
    ]
    span_rows = [row for row in rows if row[0] == "span_total"][:top]
    for _section, name, n, total_ns, p50_ns, p99_ns, max_ns, _order_ns in span_rows:
        out.append(
            f"{name[:40].ljust(40)} n={int(n):>6} "
            f"total={fmt_ns(int(total_ns)):>11} "
            f"p50={fmt_ns(int(p50_ns)):>10} "
            f"p99={fmt_ns(int(p99_ns)):>10} "
            f"max={fmt_ns(int(max_ns)):>10}"
        )
    return "\n".join(out)


def run_python_file(
    script: Path,
    tables: DerivedTables,
    input_path: Path,
    names_path: Path | None,
    parquet_dir: Path | None,
) -> subprocess.CompletedProcess[str]:
    if parquet_dir is not None:
        table_dir = parquet_dir
        export_derived_tables_parquet(tables, table_dir)
        return run_python_file_with_table_dir(script, input_path, names_path, table_dir)
    with tempfile.TemporaryDirectory() as tmpdir:
        table_dir = Path(tmpdir)
        export_derived_tables_parquet(tables, table_dir)
    return run_python_file_with_table_dir(script, input_path, names_path, table_dir)


def run_python_projection(
    script: Path,
    projection: ProjectionInput,
    input_path: Path,
    names_path: Path | None,
    parquet_dir: Path | None,
) -> subprocess.CompletedProcess[str]:
    tables = projection.derived or build_derived_tables(projection.stream.records)
    return run_python_file(script, tables, input_path, names_path, parquet_dir)


def export_projection_parquet(
    projection: ProjectionInput,
    out_dir: Path,
    *,
    input_hash: str | None = None,
    dangling_span_policy: str = "exclude",
) -> list[Path]:
    tables = projection.derived or build_derived_tables(projection.stream.records)
    return export_derived_tables_parquet(
        tables,
        out_dir,
        input_hash=input_hash,
        dangling_span_policy=dangling_span_policy,
    )


def run_python_file_with_table_dir(
    script: Path,
    input_path: Path,
    names_path: Path | None,
    table_dir: Path,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(
        {
            "TX_OBSERVE_TABLE_DIR": str(table_dir),
            "TX_OBSERVE_SPANS_PARQUET": str(table_dir / "spans.parquet"),
            "TX_OBSERVE_COUNTERS_PARQUET": str(table_dir / "counters.parquet"),
            "TX_OBSERVE_ALLOCATION_ROWS_PARQUET": str(table_dir / "allocation_rows.parquet"),
            "TX_OBSERVE_SCHED_INTERVALS_PARQUET": str(table_dir / "sched_intervals.parquet"),
            "TX_OBSERVE_LOCK_ROWS_PARQUET": str(table_dir / "lock_rows.parquet"),
            "TX_OBSERVE_DS_METHOD_ROWS_PARQUET": str(table_dir / "ds_method_rows.parquet"),
            "TX_OBSERVE_INPUT": str(input_path),
        }
    )
    if names_path is not None:
        env["TX_OBSERVE_NAMES_JSON"] = str(names_path)
    return subprocess.run([sys.executable, str(script)], capture_output=True, text=True, env=env)


def load_or_build_derived_tables(
    input_path: Path | None,
    records: list[dict[str, Any]],
    *,
    cache_dir: Path | None = None,
    dangling_span_policy: str = "exclude",
) -> DerivedCacheResult:
    if input_path is None or cache_dir is None:
        return DerivedCacheResult(
            build_derived_tables(records, dangling_span_policy=dangling_span_policy)
        )

    input_hash = file_sha256(input_path)
    cache_dir.mkdir(parents=True, exist_ok=True)
    path = derived_cache_path(cache_dir, input_hash, dangling_span_policy)
    if path.exists():
        try:
            data = json.loads(path.read_text())
            meta = data.get("meta") or {}
            if (
                meta.get("input_hash") == input_hash
                and meta.get("decoder_version") == ANALYZER_DECODER_VERSION
                and meta.get("dangling_span_policy") == dangling_span_policy
            ):
                return DerivedCacheResult(derived_tables_from_json(data), path=path, hit=True)
        except (OSError, json.JSONDecodeError):
            pass

    tables = build_derived_tables(records, dangling_span_policy=dangling_span_policy)
    tmp = path.with_suffix(path.suffix + f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(derived_tables_to_json(tables, input_hash), separators=(",", ":")))
    tmp.replace(path)
    return DerivedCacheResult(tables, path=path, hit=False)


def load_derived_tables_cache(
    input_path: Path,
    *,
    cache_dir: Path,
    dangling_span_policy: str,
) -> DerivedCacheResult | None:
    input_hash = file_sha256(input_path)
    path = derived_cache_path(cache_dir, input_hash, dangling_span_policy)
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    meta = data.get("meta") or {}
    if (
        meta.get("input_hash") != input_hash
        or meta.get("decoder_version") != ANALYZER_DECODER_VERSION
        or meta.get("dangling_span_policy") != dangling_span_policy
    ):
        return None
    return DerivedCacheResult(derived_tables_from_json(data), path=path, hit=True)
