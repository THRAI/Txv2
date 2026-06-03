#!/usr/bin/env python3
"""Render a full tx-observe DS zone/method report from derived Parquet tables."""

from __future__ import annotations

import argparse
import csv
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


DURATION_METRIC = 3848926430  # fnv1a32("debug.ds.method.duration_ns")
RESERVE_FOR = 2588343616  # fnv1a32("debug.ds.substrate.zone.reserve_for")
POP_FREE_SLOT = 2826410679  # fnv1a32("debug.ds.substrate.zone.pop_free_slot")


def duckdb_sql_string(value: str | Path) -> str:
    text = str(value)
    return "'" + text.replace("'", "''") + "'"


def names_values(names_json: Path | None) -> str:
    if names_json is None or not names_json.exists():
        return "(SELECT NULL::UINTEGER AS id, NULL::VARCHAR AS name WHERE false)"
    data = json.loads(names_json.read_text()).get("name_table", {})
    if not data:
        return "(SELECT NULL::UINTEGER AS id, NULL::VARCHAR AS name WHERE false)"
    values = ", ".join(
        f"({int(name_id)}, {duckdb_sql_string(str(name))})"
        for name_id, name in sorted(data.items(), key=lambda item: int(item[0]))
    )
    return f"(VALUES {values}) AS t(id, name)"


def run_csv_query(sql: str) -> list[dict[str, str]]:
    duckdb = shutil.which("duckdb")
    if duckdb is None:
        raise RuntimeError("duckdb CLI is required")
    proc = subprocess.run(
        [duckdb, "--no-stdin", "-csv", "-c", sql],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or proc.stdout.strip())
    return list(csv.DictReader(proc.stdout.splitlines()))


def setup_sql(ds_method_rows: Path, names_json: Path | None) -> str:
    return f"""
CREATE TEMP TABLE names AS
SELECT CAST(id AS UINTEGER) AS id, CAST(name AS VARCHAR) AS name
FROM {names_values(names_json)};

CREATE TEMP VIEW ds_method_rows AS
SELECT * FROM read_parquet({duckdb_sql_string(ds_method_rows)});
"""


def query_grouped(ds_method_rows: Path, names_json: Path | None, where: str = "true") -> list[dict[str, str]]:
    sql = (
        setup_sql(ds_method_rows, names_json)
        + f"""
SELECT
  coalesce(nz.name, 'zone_0x' || lower(hex(d.zone_id))) AS zone,
  coalesce(nm.name, 'method_0x' || lower(hex(d.method_id))) AS method,
  count(*) AS n,
  sum(value) AS total_ns,
  quantile_disc(value, 0.50) AS p50_ns,
  quantile_disc(value, 0.95) AS p95_ns,
  quantile_disc(value, 0.99) AS p99_ns,
  max(value) AS max_ns
FROM ds_method_rows d
LEFT JOIN names nz ON nz.id = d.zone_id
LEFT JOIN names nm ON nm.id = d.method_id
WHERE d.metric_id = {DURATION_METRIC} AND {where}
GROUP BY zone, method
ORDER BY total_ns DESC, zone, method;
"""
    )
    return run_csv_query(sql)


def query_by_zone(ds_method_rows: Path, names_json: Path | None) -> list[dict[str, str]]:
    sql = (
        setup_sql(ds_method_rows, names_json)
        + f"""
SELECT
  coalesce(nz.name, 'zone_0x' || lower(hex(d.zone_id))) AS zone,
  count(*) AS n,
  sum(value) AS total_ns,
  quantile_disc(value, 0.50) AS p50_ns,
  quantile_disc(value, 0.95) AS p95_ns,
  quantile_disc(value, 0.99) AS p99_ns,
  max(value) AS max_ns
FROM ds_method_rows d
LEFT JOIN names nz ON nz.id = d.zone_id
WHERE d.metric_id = {DURATION_METRIC}
GROUP BY zone
ORDER BY total_ns DESC, zone;
"""
    )
    return run_csv_query(sql)


def query_by_method(ds_method_rows: Path, names_json: Path | None) -> list[dict[str, str]]:
    sql = (
        setup_sql(ds_method_rows, names_json)
        + f"""
SELECT
  coalesce(nm.name, 'method_0x' || lower(hex(d.method_id))) AS method,
  count(*) AS n,
  sum(value) AS total_ns,
  quantile_disc(value, 0.50) AS p50_ns,
  quantile_disc(value, 0.95) AS p95_ns,
  quantile_disc(value, 0.99) AS p99_ns,
  max(value) AS max_ns
FROM ds_method_rows d
LEFT JOIN names nm ON nm.id = d.method_id
WHERE d.metric_id = {DURATION_METRIC}
GROUP BY method
ORDER BY total_ns DESC, method;
"""
    )
    return run_csv_query(sql)


def format_ns(value: str) -> str:
    ns = int(value)
    if ns >= 1_000_000_000:
        return f"{ns / 1_000_000_000:.3f}s"
    if ns >= 1_000_000:
        return f"{ns / 1_000_000:.3f}ms"
    if ns >= 1_000:
        return f"{ns / 1_000:.3f}us"
    return f"{ns}ns"


def table(headers: list[str], rows: list[dict[str, str]]) -> str:
    out = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    for row in rows:
        values = []
        for header in headers:
            value = row[header]
            if header.endswith("_ns"):
                value = format_ns(value)
            values.append(value)
        out.append("| " + " | ".join(values) + " |")
    return "\n".join(out)


def write_csv(path: Path, rows: list[dict[str, str]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if not rows:
        path.write_text("")
        return
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)


def render_report(ds_method_rows: Path, names_json: Path | None, csv_dir: Path | None) -> str:
    by_zone = query_by_zone(ds_method_rows, names_json)
    by_method = query_by_method(ds_method_rows, names_json)
    full = query_grouped(ds_method_rows, names_json)
    reserve_for = query_grouped(ds_method_rows, names_json, f"d.method_id = {RESERVE_FOR}")
    pop_free_slot = query_grouped(ds_method_rows, names_json, f"d.method_id = {POP_FREE_SLOT}")

    if csv_dir is not None:
        write_csv(csv_dir / "ds-zone-summary.csv", by_zone)
        write_csv(csv_dir / "ds-method-summary.csv", by_method)
        write_csv(csv_dir / "ds-zone-method-full.csv", full)
        write_csv(csv_dir / "ds-zone-reserve-for.csv", reserve_for)
        write_csv(csv_dir / "ds-zone-pop-free-slot.csv", pop_free_slot)

    lines = [
        "# tx-observe DS Zone Full Report",
        "",
        f"- Source: `{ds_method_rows}`",
        f"- Names: `{names_json}`" if names_json else "- Names: none",
        f"- Zone/method groups: `{len(full)}`",
        "",
        "## By Zone",
        "",
        table(["zone", "n", "total_ns", "p50_ns", "p95_ns", "p99_ns", "max_ns"], by_zone),
        "",
        "## By Method",
        "",
        table(["method", "n", "total_ns", "p50_ns", "p95_ns", "p99_ns", "max_ns"], by_method),
        "",
        "## Allocation Reservation",
        "",
        table(["zone", "method", "n", "total_ns", "p50_ns", "p95_ns", "p99_ns", "max_ns"], reserve_for),
        "",
        "## Free-Slot Pop",
        "",
        table(["zone", "method", "n", "total_ns", "p50_ns", "p95_ns", "p99_ns", "max_ns"], pop_free_slot),
        "",
        "## Full Zone x Method Table",
        "",
        table(["zone", "method", "n", "total_ns", "p50_ns", "p95_ns", "p99_ns", "max_ns"], full),
        "",
    ]
    if csv_dir is not None:
        lines.extend(["CSV artifacts:", "", f"- `{csv_dir}`", ""])
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet-dir", type=Path)
    parser.add_argument("--ds-method-rows", type=Path)
    parser.add_argument("--names", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--csv-dir", type=Path)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    ds_method_rows = args.ds_method_rows
    if ds_method_rows is None and args.parquet_dir is not None:
        ds_method_rows = args.parquet_dir / "ds_method_rows.parquet"
    if ds_method_rows is None:
        env_path = os.environ.get("TX_OBSERVE_DS_METHOD_ROWS_PARQUET")
        if env_path:
            ds_method_rows = Path(env_path)
    if ds_method_rows is None:
        raise RuntimeError("--ds-method-rows, --parquet-dir, or TX_OBSERVE_DS_METHOD_ROWS_PARQUET is required")

    names = args.names
    if names is None and os.environ.get("TX_OBSERVE_NAMES_JSON"):
        names = Path(os.environ["TX_OBSERVE_NAMES_JSON"])

    csv_dir = args.csv_dir
    if csv_dir is None and args.output is not None:
        csv_dir = args.output.with_suffix("").parent / (args.output.with_suffix("").name + "-csv")
    if csv_dir is None and os.environ.get("TX_OBSERVE_TABLE_DIR"):
        csv_dir = Path(os.environ["TX_OBSERVE_TABLE_DIR"]) / "ds-zone-report-csv"

    report = render_report(ds_method_rows, names, csv_dir)
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(report)
    else:
        print(report)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
