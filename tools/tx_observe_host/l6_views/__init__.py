"""L6 projection input types for Python observe tooling."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ..l5_canonical import TraceEventStream

HOST_CATALOG_PATH = Path(__file__).resolve().parents[2] / "tx-observe-host-catalog.json"


class ProjectionInput:
    def __init__(
        self,
        stream: TraceEventStream,
        names: dict[int, str],
        derived: Any | None = None,
    ) -> None:
        self.stream = stream
        self.names = names
        self.derived = derived


def duckdb_sql_string(value: Path | str) -> str:
    text = value.as_posix() if isinstance(value, Path) else value
    return "'" + text.replace("'", "''") + "'"


def duckdb_json_columns(schema: list[tuple[str, str]]) -> str:
    fields = ", ".join(f"'{column}':'VARCHAR'" for column, _ in schema)
    return "{" + fields + "}"


def read_json_typed(path: Path, schema: list[tuple[str, str]]) -> str:
    return (
        "read_json("
        + duckdb_sql_string(path)
        + f", columns={duckdb_json_columns(schema)}, format='newline_delimited')"
    )


def load_host_catalog(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def projection_schemas_from_host_catalog(catalog: dict[str, Any]) -> dict[str, list[tuple[str, str]]]:
    out: dict[str, list[tuple[str, str]]] = {}
    for projection in catalog.get("projections") or []:
        columns = projection.get("columns") or []
        if not columns:
            continue
        kind = projection.get("kind")
        if kind == "derived_table":
            key = projection.get("file")
        elif kind == "sql_view":
            key = projection.get("id")
        else:
            continue
        if not isinstance(key, str):
            raise ValueError(f"host catalog projection is missing key: {projection!r}")
        out[key] = [(str(column["name"]), str(column["type"])) for column in columns]
    return out


def validate_host_catalog_topology(catalog: dict[str, Any]) -> dict[str, dict[str, Any]]:
    control_groups = {
        str(group["id"]): group
        for group in catalog.get("control_groups") or []
        if isinstance(group, dict) and "id" in group
    }
    projections = {
        str(projection["id"]): projection
        for projection in catalog.get("projections") or []
        if isinstance(projection, dict) and "id" in projection
    }
    event_families = {
        str(family["id"]): family
        for family in catalog.get("event_families") or []
        if isinstance(family, dict) and "id" in family
    }
    if not event_families:
        raise ValueError("host catalog is missing event_families")
    allowed_control_groups = set(control_groups) | {
        "always_on_when_observe_enabled",
        "always_on_when_callsite_enabled",
    }
    missing: list[str] = []
    for family_id, family in event_families.items():
        control_group = family.get("control_group")
        if control_group is not None and str(control_group) not in allowed_control_groups:
            missing.append(f"{family_id} control_group -> {control_group}")
        for projection in family.get("projection") or []:
            if str(projection) not in projections:
                missing.append(f"{family_id} projection -> {projection}")
    if missing:
        raise ValueError(f"host catalog topology mismatch: missing={missing}")
    return {
        "control_groups": control_groups,
        "projections": projections,
        "event_families": event_families,
    }


def validate_host_catalog(path: Path = HOST_CATALOG_PATH) -> dict[str, list[tuple[str, str]]]:
    catalog = load_host_catalog(path)
    validate_host_catalog_topology(catalog)
    catalog_schemas = projection_schemas_from_host_catalog(catalog)
    return catalog_schemas


_PROJECTION_SCHEMA_CATALOG: dict[str, list[tuple[str, str]]] | None = None


def projection_schema_catalog(path: Path | None = None) -> dict[str, list[tuple[str, str]]]:
    global _PROJECTION_SCHEMA_CATALOG
    catalog_path = path or HOST_CATALOG_PATH
    if catalog_path == HOST_CATALOG_PATH and _PROJECTION_SCHEMA_CATALOG is not None:
        return _PROJECTION_SCHEMA_CATALOG
    schemas = validate_host_catalog(catalog_path)
    if catalog_path == HOST_CATALOG_PATH:
        _PROJECTION_SCHEMA_CATALOG = schemas
    return schemas


def parquet_schema_catalog() -> dict[str, list[tuple[str, str]]]:
    return {
        filename: schema
        for filename, schema in projection_schema_catalog().items()
        if filename.endswith(".parquet")
    }


def parquet_select_sql(
    filename: str,
    json_path: Path,
    rows: list[dict[str, Any]],
) -> str:
    schema = projection_schema_catalog()[filename]
    select_list = ", ".join(
        f'CAST("{column}" AS {duck_type}) AS "{column}"'
        for column, duck_type in schema
    )
    if rows:
        return f"SELECT {select_list} FROM {read_json_typed(json_path, schema)}"
    empty_select = ", ".join(
        f'CAST(NULL AS {duck_type}) AS "{column}"'
        for column, duck_type in schema
    )
    return f"SELECT {empty_select} WHERE false"


def typed_select_sql(
    json_path: Path,
    rows: list[dict[str, Any]],
    schema: list[tuple[str, str]],
) -> str:
    select_list = ", ".join(
        f'CAST("{column}" AS {duck_type}) AS "{column}"'
        for column, duck_type in schema
    )
    if rows:
        return f"SELECT {select_list} FROM {read_json_typed(json_path, schema)}"
    empty_select = ", ".join(
        f'CAST(NULL AS {duck_type}) AS "{column}"'
        for column, duck_type in schema
    )
    return f"SELECT {empty_select} WHERE false"


from .tables import (  # noqa: E402
    ANALYZER_DECODER_VERSION,
    DS_METHOD_TRACK_ID,
    LOCK_TRACK_ID,
    PARQUET_MANIFEST,
    DerivedCacheResult,
    DerivedTables,
    analyze_parquet_summary,
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
)
from .reports import (  # noqa: E402
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
)
