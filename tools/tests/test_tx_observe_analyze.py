import importlib.util
import json
import contextlib
import io
import shutil
import subprocess
import struct
import sys
import unittest
from tempfile import NamedTemporaryFile, TemporaryDirectory
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "tx-observe-analyze.py"
SPEC = importlib.util.spec_from_file_location("tx_observe_analyze", SCRIPT)
assert SPEC is not None
analyzer = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(analyzer)


def make_record(
    *,
    magic: int = analyzer.RECORD_MAGIC,
    version: int = analyzer.SUPPORTED_RECORD_VERSION,
    kind: int = 13,
    level: int = 6,
    hart: int = 0,
    seq: int = 1,
    ts: int = 100,
    span: int = 0,
    parent: int = 0,
    name: int = 0,
    payload_tag: int = analyzer.PAYLOAD_COUNTER_VALUE,
    payload_len: int = 16,
    payload: bytes | None = None,
) -> bytes:
    payload = payload if payload is not None else struct.pack("<IIQ", 1, 0, 2)
    return analyzer.RECORD_STRUCT.pack(
        magic,
        version,
        kind,
        level,
        0,
        0,
        0,
        hart,
        0,
        0,
        seq,
        ts,
        span,
        parent,
        name,
        payload_tag,
        payload_len,
        payload,
        b"\x00" * 8,
    )


class TxObserveAnalyzeTests(unittest.TestCase):
    def test_analyze_accepts_string_timestamps_and_reports_hart_local_gaps(self) -> None:
        records = [
            {"kind": "Counter", "hart": 0, "ts": "0", "payload": {"counter_id": 1, "value": "9007199254740993"}},
            {"kind": "Counter", "hart": 1, "ts": "1000", "payload": {"counter_id": 1, "value": 7}},
            {"kind": "Counter", "hart": 1, "ts": "1100", "payload": {"counter_id": 1, "value": 7}},
            {"kind": "Counter", "hart": 0, "ts": "5000", "payload": {"counter_id": 1, "value": "9007199254740993"}},
        ]

        report = analyzer.analyze(records, {1: "debug.test.counter"}, top=1)

        self.assertIn("records=4 window=5.0us", report)
        self.assertIn("hart=0 dt=5.0us", report)
        self.assertNotIn("hart=1 dt=0.1us", report)
        self.assertIn("debug.test.counter: 9007199254740993:2", report)

    def test_allocation_rows_accept_stringified_wide_payload_value(self) -> None:
        name_id = analyzer.fnv1a32("debug.alloc.sample.bytes")
        records = [
            {
                "kind": "Instant",
                "payload_tag": 40,
                "parent": str(0xD500_0000_0000_0001),
                "name_id": name_id,
                "ts": "9007199254740993",
                "payload": {"value0": "9007199254740995"},
            }
        ]

        rows = analyzer.allocation_rows(records, {name_id: "debug.alloc.sample.bytes"})

        self.assertEqual(
            rows,
            [
                (
                    9007199254740993,
                    "debug.alloc.zone.slab",
                    "debug.alloc.sample.bytes",
                    9007199254740995,
                )
            ],
        )

    def test_span_summary_reports_net_cross_hart_close(self) -> None:
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "72057594037927937", "payload": {}},
            {"kind": "SpanEnd", "hart": 1, "ts": "300", "span": "72057594037927937", "payload": {}},
        ]

        report = analyzer.analyze(records, {}, top=1)

        self.assertIn("harts=0->1 net_migrated=true", report)

    def test_dangling_span_policy_is_explicit(self) -> None:
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "Counter", "hart": 0, "ts": "500", "payload": {"counter_id": 1, "value": 1}},
        ]

        excluded = analyzer.build_derived_tables(records, dangling_span_policy="exclude")
        self.assertEqual(excluded.meta["dangling_span_count"], 1)
        self.assertEqual(excluded.meta["dangling_spans_materialized"], 0)
        self.assertEqual(excluded.spans, [])

        synthetic = analyzer.build_derived_tables(records, dangling_span_policy="synthetic-end")
        self.assertEqual(synthetic.meta["dangling_span_count"], 1)
        self.assertEqual(synthetic.meta["dangling_spans_materialized"], 1)
        self.assertEqual(synthetic.spans[0]["dur"], 400)
        self.assertTrue(synthetic.spans[0]["dangling"])

        report = analyzer.analyze(records, {7: "debug.open"}, top=1, derived=synthetic)
        self.assertIn("dangling_policy=synthetic-end dangling_materialized=1", report)
        self.assertIn("dangling=true", report)

    def test_derived_cache_keys_on_input_hash_and_decoder_version(self) -> None:
        with TemporaryDirectory() as tmpdir:
            cache_dir = Path(tmpdir) / "cache"
            trace = Path(tmpdir) / "trace.ndjson"
            trace.write_text(
                json.dumps({"kind": "Counter", "hart": 0, "ts": "1", "payload": {"counter_id": 1, "value": 1}})
                + "\n"
            )
            records = analyzer.load_records(trace)

            first = analyzer.load_or_build_derived_tables(trace, records, cache_dir=cache_dir)
            second = analyzer.load_or_build_derived_tables(trace, records, cache_dir=cache_dir)

            self.assertFalse(first.hit)
            self.assertTrue(second.hit)
            self.assertEqual(first.path, second.path)
            self.assertEqual(second.tables.meta["decoder_version"], analyzer.ANALYZER_DECODER_VERSION)

            trace.write_text(
                json.dumps({"kind": "Counter", "hart": 0, "ts": "1", "payload": {"counter_id": 1, "value": 2}})
                + "\n"
            )
            changed_records = analyzer.load_records(trace)
            changed = analyzer.load_or_build_derived_tables(trace, changed_records, cache_dir=cache_dir)

            self.assertFalse(changed.hit)
            self.assertNotEqual(first.path, changed.path)

    def test_export_derived_tables_writes_parquet_files(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet export")
        with TemporaryDirectory() as tmpdir:
            out_dir = Path(tmpdir) / "parquet"
            records = [
                {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
                {"kind": "Counter", "hart": 0, "ts": "150", "payload": {"counter_id": 3, "value": 9}},
                {
                    "kind": "Instant",
                    "hart": 0,
                    "ts": "175",
                    "payload_tag": analyzer.PAYLOAD_ARG_VALUE,
                    "parent": str(0xD500_0000_0000_0001),
                    "name_id": analyzer.fnv1a32("debug.alloc.sample.bytes"),
                    "payload": {"value0": 11},
                },
                {
                    "kind": "Instant",
                    "hart": 0,
                    "ts": "190",
                    "payload_tag": analyzer.PAYLOAD_ARG_VALUE,
                    "parent": str(analyzer.LOCK_TRACK_ID),
                    "name_id": analyzer.fnv1a32("debug.lock.test"),
                    "payload": {
                        "key": analyzer.fnv1a32("debug.lock.wait_ns"),
                        "value0": 17,
                    },
                },
                {"kind": "SpanEnd", "hart": 0, "ts": "300", "span": "0x1", "payload": {}},
            ]
            tables = analyzer.build_derived_tables(records)

            written = analyzer.export_derived_tables_parquet(tables, out_dir)

            self.assertEqual(
                {path.name for path in written},
                {
                    "spans.parquet",
                    "counters.parquet",
                    "allocation_rows.parquet",
                    "sched_intervals.parquet",
                    "lock_rows.parquet",
                },
            )
            for path in written:
                data = path.read_bytes()
                self.assertTrue(data.startswith(b"PAR1"), path)
                self.assertTrue(data.endswith(b"PAR1"), path)
                self.assertGreater(len(data), 12, path)
            self.assertFalse(list(out_dir.glob("*.jsonl")))

            span_rows = subprocess.run(
                [
                    duckdb,
                    "-csv",
                    "-c",
                    f"SELECT span,dur FROM read_parquet('{(out_dir / 'spans.parquet').as_posix()}')",
                ],
                capture_output=True,
                check=True,
                text=True,
            ).stdout.strip()
            counter_rows = subprocess.run(
                [
                    duckdb,
                    "-csv",
                    "-c",
                    f"SELECT counter_id,value FROM read_parquet('{(out_dir / 'counters.parquet').as_posix()}')",
                ],
                capture_output=True,
                check=True,
                text=True,
            ).stdout.strip()
            allocation_rows = subprocess.run(
                [
                    duckdb,
                    "-csv",
                    "-c",
                    (
                        "SELECT track,value FROM "
                        f"read_parquet('{(out_dir / 'allocation_rows.parquet').as_posix()}')"
                    ),
                ],
                capture_output=True,
                check=True,
                text=True,
            ).stdout.strip()
            lock_rows = subprocess.run(
                [
                    duckdb,
                    "-csv",
                    "-c",
                    (
                        "SELECT lock_id,metric_id,value FROM "
                        f"read_parquet('{(out_dir / 'lock_rows.parquet').as_posix()}')"
                    ),
                ],
                capture_output=True,
                check=True,
                text=True,
            ).stdout.strip()

            self.assertEqual(span_rows, "span,dur\n0x1,200")
            self.assertEqual(counter_rows, "counter_id,value\n3,9")
            self.assertEqual(allocation_rows, "track,value\ndebug.alloc.zone.slab,11")
            self.assertEqual(
                lock_rows,
                "lock_id,metric_id,value\n"
                f"{analyzer.fnv1a32('debug.lock.test')},{analyzer.fnv1a32('debug.lock.wait_ns')},17",
            )

    def test_export_derived_tables_writes_empty_parquet_files(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet export")
        with TemporaryDirectory() as tmpdir:
            out_dir = Path(tmpdir) / "parquet"
            tables = analyzer.DerivedTables(
                spans=[],
                counters=[],
                allocation_rows=[],
                sched_intervals=[],
                lock_rows=[],
                meta={},
            )

            analyzer.export_derived_tables_parquet(tables, out_dir)

            for filename in [
                "spans.parquet",
                "counters.parquet",
                "allocation_rows.parquet",
                "sched_intervals.parquet",
                "lock_rows.parquet",
            ]:
                path = out_dir / filename
                data = path.read_bytes()
                self.assertTrue(data.startswith(b"PAR1"), path)
                self.assertTrue(data.endswith(b"PAR1"), path)
                count_rows = subprocess.run(
                    [duckdb, "-csv", "-c", f"SELECT count(*) FROM read_parquet('{path.as_posix()}')"],
                    capture_output=True,
                    check=True,
                    text=True,
                ).stdout.strip()
                self.assertEqual(count_rows, "count_star()\n0")

    def test_export_derived_tables_preserves_wide_unsigned_values(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet export")
        with TemporaryDirectory() as tmpdir:
            out_dir = Path(tmpdir) / "parquet"
            tables = analyzer.DerivedTables(
                spans=[],
                counters=[
                    {
                        "ts": 1,
                        "counter_id": 2,
                        "value": 18_446_744_071_564_761_610,
                    }
                ],
                allocation_rows=[],
                sched_intervals=[],
                lock_rows=[],
                meta={},
            )

            analyzer.export_derived_tables_parquet(tables, out_dir)

            value_rows = subprocess.run(
                [
                    duckdb,
                    "-csv",
                    "-c",
                    f"SELECT value::VARCHAR FROM read_parquet('{(out_dir / 'counters.parquet').as_posix()}')",
                ],
                capture_output=True,
                check=True,
                text=True,
            ).stdout.strip()
            self.assertTrue(value_rows.endswith("\n18446744071564761610"), value_rows)

    def test_parquet_text_summary_uses_derived_tables(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet summary")
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "SpanEnd", "hart": 0, "ts": "300", "span": "0x1", "payload": {}},
            {"kind": "Counter", "hart": 0, "ts": "350", "payload": {"counter_id": 3, "value": 9}},
        ]
        tables = analyzer.build_derived_tables(records)
        with TemporaryDirectory() as tmpdir:
            parquet_dir = Path(tmpdir) / "parquet"
            analyzer.export_derived_tables_parquet(tables, parquet_dir)

            report = analyzer.analyze_parquet_summary(parquet_dir, {7: "debug.span", 3: "debug.counter"}, top=5)

            self.assertIn("derived parquet summary:", report)
            self.assertIn("spans=1 counters=1 allocation_rows=0 sched_intervals=0 lock_rows=0", report)
            self.assertIn("debug.span", report)
            self.assertIn("p50=     0.2us", report)

    def test_parquet_text_summary_can_load_derived_cache_before_records(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet summary")
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "SpanEnd", "hart": 0, "ts": "300", "span": "0x1", "payload": {}},
        ]
        with TemporaryDirectory() as tmpdir:
            tmp = Path(tmpdir)
            rawrecords = tmp / "trace.rawrecords"
            rawrecords.write_bytes(b"cached-input")
            cache_dir = tmp / "cache"
            parquet_dir = tmp / "parquet"
            input_hash = analyzer.file_sha256(rawrecords)
            cache_dir.mkdir()
            cache_path = analyzer.derived_cache_path(cache_dir, input_hash, "exclude")
            tables = analyzer.build_derived_tables(records)
            cache_path.write_text(json.dumps(analyzer.derived_tables_to_json(tables, input_hash)))

            proc = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--rawrecords",
                    str(rawrecords),
                    "--cache-dir",
                    str(cache_dir),
                    "--parquet-dir",
                    str(parquet_dir),
                    "--source-root",
                    "/dev/null",
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("derived cache: hit", proc.stdout)
            self.assertIn("derived parquet summary:", proc.stdout)
            self.assertIn("spans=1 counters=0 allocation_rows=0 sched_intervals=0 lock_rows=0", proc.stdout)

    def test_parquet_text_summary_reuses_matching_parquet_manifest(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Parquet summary")
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "SpanEnd", "hart": 0, "ts": "300", "span": "0x1", "payload": {}},
        ]
        with TemporaryDirectory() as tmpdir:
            tmp = Path(tmpdir)
            rawrecords = tmp / "trace.rawrecords"
            rawrecords.write_bytes(b"cached-input")
            cache_dir = tmp / "cache"
            parquet_dir = tmp / "parquet"
            tables = analyzer.build_derived_tables(records)
            input_hash = analyzer.file_sha256(rawrecords)
            analyzer.export_derived_tables_parquet(
                tables,
                parquet_dir,
                input_hash=input_hash,
                dangling_span_policy="exclude",
            )

            proc = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--rawrecords",
                    str(rawrecords),
                    "--cache-dir",
                    str(cache_dir),
                    "--parquet-dir",
                    str(parquet_dir),
                    "--source-root",
                    "/dev/null",
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("parquet cache: hit", proc.stdout)
            self.assertNotIn("derived cache:", proc.stdout)
            self.assertIn("derived parquet summary:", proc.stdout)
            self.assertIn("spans=1 counters=0 allocation_rows=0 sched_intervals=0 lock_rows=0", proc.stdout)

    def test_lock_rows_are_derived_from_lock_track_arg_values(self) -> None:
        lock_id = analyzer.fnv1a32("debug.lock.process.fds")
        wait_id = analyzer.fnv1a32("debug.lock.wait_ns")
        service_id = analyzer.fnv1a32("debug.lock.service_ns")
        records = [
            {
                "kind": "Instant",
                "hart": 2,
                "ts": "100",
                "payload_tag": analyzer.PAYLOAD_ARG_VALUE,
                "parent": str(analyzer.LOCK_TRACK_ID),
                "name_id": lock_id,
                "payload": {"key": wait_id, "value0": "9007199254740993"},
            },
            {
                "kind": "Instant",
                "hart": 2,
                "ts": "140",
                "payload_tag": analyzer.PAYLOAD_ARG_VALUE,
                "parent": str(analyzer.LOCK_TRACK_ID),
                "name_id": lock_id,
                "payload": {"key": service_id, "value0": 40},
            },
        ]

        tables = analyzer.build_derived_tables(records)

        self.assertEqual(
            tables.lock_rows,
            [
                {
                    "ts": 100,
                    "hart": 2,
                    "lock_id": lock_id,
                    "metric_id": wait_id,
                    "value": 9007199254740993,
                },
                {
                    "ts": 140,
                    "hart": 2,
                    "lock_id": lock_id,
                    "metric_id": service_id,
                    "value": 40,
                },
            ],
        )
        self.assertEqual(tables.meta["lock_row_count"], 2)

        report = analyzer.analyze_lock_metrics(
            records,
            {
                lock_id: "debug.lock.process.fds",
                wait_id: "debug.lock.wait_ns",
                service_id: "debug.lock.service_ns",
            },
            top=1,
            derived=tables,
        )
        self.assertIn("lock metrics:", report)
        self.assertIn("debug.lock.process.fds", report)
        self.assertIn("wait n=1", report)
        self.assertIn("service p50=0.0us", report)
        self.assertIn("rho=", report)

    def test_sched_join_detects_migration_even_when_span_starts_and_ends_on_same_hart(self) -> None:
        records = [
            {
                "kind": "SpanBegin",
                "level": "Sched",
                "hart": 0,
                "ts": "50",
                "span": "0x10",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 0,
                    "kind": 0,
                    "reason": 0,
                },
            },
            {
                "kind": "SpanBegin",
                "level": "Drive",
                "hart": 0,
                "ts": "100",
                "span": "0x20",
                "name_id": analyzer.fnv1a32("debug.drive.sample"),
                "payload": {},
            },
            {
                "kind": "SpanEnd",
                "level": "Sched",
                "hart": 0,
                "ts": "150",
                "span": "0x10",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 0,
                    "kind": 1,
                    "reason": 1,
                },
            },
            {
                "kind": "SpanBegin",
                "level": "Sched",
                "hart": 1,
                "ts": "160",
                "span": "0x11",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 1,
                    "kind": 0,
                    "reason": 0,
                },
            },
            {
                "kind": "SpanEnd",
                "level": "Sched",
                "hart": 1,
                "ts": "220",
                "span": "0x11",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 1,
                    "kind": 1,
                    "reason": 1,
                },
            },
            {
                "kind": "SpanBegin",
                "level": "Sched",
                "hart": 0,
                "ts": "230",
                "span": "0x12",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 0,
                    "kind": 0,
                    "reason": 0,
                },
            },
            {
                "kind": "SpanEnd",
                "level": "Drive",
                "hart": 0,
                "ts": "300",
                "span": "0x20",
                "payload": {},
            },
            {
                "kind": "SpanEnd",
                "level": "Sched",
                "hart": 0,
                "ts": "320",
                "span": "0x12",
                "payload_tag": 53,
                "payload": {
                    "task_id_low": 7,
                    "process_id_low": 42,
                    "hart_id": 0,
                    "kind": 1,
                    "reason": 1,
                },
            },
        ]

        derived = analyzer.build_derived_tables(records)
        span = next(row for row in derived.spans if row["span"] == "0x20")

        self.assertEqual(
            derived.sched_intervals,
            [
                {"task": 7, "pid": 42, "hart": 0, "begin": 50, "end": 150, "dur": 100},
                {"task": 7, "pid": 42, "hart": 1, "begin": 160, "end": 220, "dur": 60},
                {"task": 7, "pid": 42, "hart": 0, "begin": 230, "end": 320, "dur": 90},
            ],
        )
        self.assertEqual(span["begin_hart"], 0)
        self.assertEqual(span["end_hart"], 0)
        self.assertEqual(span.get("task_id"), 7)
        self.assertEqual(span.get("process_id"), 42)
        self.assertEqual(span.get("sched_harts"), [0, 1])
        self.assertEqual(span.get("sched_segments"), 3)
        self.assertTrue(span.get("sched_migrated"))

        report = analyzer.analyze(records, {}, top=1, derived=derived)
        self.assertIn("harts=0->0 net_migrated=false", report)
        self.assertIn("sched_harts=0->1 sched_migrated=true", report)

    def test_load_txtrace_records_merges_per_hart_runs(self) -> None:
        def make_counter_record(hart: int, seq: int, ts: int, counter_id: int, value: int) -> bytes:
            payload = struct.pack("<IIQ", counter_id, 0, value)
            return make_record(hart=hart, seq=seq, ts=ts, payload_tag=analyzer.PAYLOAD_COUNTER_VALUE, payload=payload)

        def make_trace(records_by_hart: list[list[bytes]]) -> Path:
            ring_order = 2
            slot_count = 1 << ring_order
            ring_data_size = analyzer.RING_HEADER_SIZE + slot_count * analyzer.SUPPORTED_RECORD_SIZE
            total = 72 + len(records_by_hart) * ring_data_size
            buf = bytearray(total)
            struct.pack_into("<I", buf, 0, analyzer.TX_TRACE_MAGIC)
            struct.pack_into("<H", buf, 4, 0)
            struct.pack_into("<H", buf, 6, 72)
            buf[8] = 1
            buf[9] = 8
            struct.pack_into("<H", buf, 10, analyzer.SUPPORTED_RECORD_SIZE)
            struct.pack_into("<H", buf, 12, len(records_by_hart))
            buf[14] = ring_order
            struct.pack_into("<Q", buf, 64, 72)

            for hart, records in enumerate(records_by_hart):
                ring_base = 72 + hart * ring_data_size
                struct.pack_into("<H", buf, ring_base, hart)
                struct.pack_into("<Q", buf, ring_base + analyzer.RING_PRODUCER_OFF, len(records))
                slots_base = ring_base + analyzer.RING_HEADER_SIZE
                for idx, record in enumerate(records):
                    off = slots_base + idx * analyzer.SUPPORTED_RECORD_SIZE
                    buf[off : off + len(record)] = record

            tmp = NamedTemporaryFile(delete=False)
            tmp.write(buf)
            tmp.flush()
            tmp.close()
            return Path(tmp.name)

        trace_path = make_trace(
            [
                [make_counter_record(0, 1, 200, 1, 2)],
                [make_counter_record(1, 1, 100, 1, 1), make_counter_record(1, 2, 300, 1, 3)],
            ]
        )
        try:
            records = analyzer.load_txtrace_records(trace_path)
        finally:
            trace_path.unlink(missing_ok=True)

        self.assertEqual([record["hart"] for record in records], [1, 0, 1])
        self.assertEqual([record["ts"] for record in records], [100, 200, 300])
        self.assertEqual(records[0]["payload"]["value"], 1)

    def test_load_txtrace_records_emits_repairs_for_malformed_slots(self) -> None:
        def make_trace(records_by_hart: list[list[bytes]]) -> Path:
            ring_order = 2
            slot_count = 1 << ring_order
            ring_data_size = analyzer.RING_HEADER_SIZE + slot_count * analyzer.SUPPORTED_RECORD_SIZE
            total = 72 + len(records_by_hart) * ring_data_size
            buf = bytearray(total)
            struct.pack_into("<I", buf, 0, analyzer.TX_TRACE_MAGIC)
            struct.pack_into("<H", buf, 4, 0)
            struct.pack_into("<H", buf, 6, 72)
            buf[8] = 1
            buf[9] = 8
            struct.pack_into("<H", buf, 10, analyzer.SUPPORTED_RECORD_SIZE)
            struct.pack_into("<H", buf, 12, len(records_by_hart))
            buf[14] = ring_order
            struct.pack_into("<Q", buf, 64, 72)

            for hart, records in enumerate(records_by_hart):
                ring_base = 72 + hart * ring_data_size
                struct.pack_into("<H", buf, ring_base, hart)
                struct.pack_into("<Q", buf, ring_base + analyzer.RING_PRODUCER_OFF, len(records))
                slots_base = ring_base + analyzer.RING_HEADER_SIZE
                for idx, record in enumerate(records):
                    off = slots_base + idx * analyzer.SUPPORTED_RECORD_SIZE
                    buf[off : off + len(record)] = record

            tmp = NamedTemporaryFile(delete=False)
            tmp.write(buf)
            tmp.flush()
            tmp.close()
            return Path(tmp.name)

        trace_path = make_trace(
            [
                [
                    make_record(
                        magic=0xDEAD,
                        seq=1,
                        ts=100,
                        payload_tag=analyzer.PAYLOAD_NONE,
                        payload_len=0,
                        payload=b"\x00" * 16,
                    ),
                    make_record(
                        version=1,
                        seq=2,
                        ts=200,
                        payload_tag=analyzer.PAYLOAD_NONE,
                        payload_len=0,
                        payload=b"\x00" * 16,
                    ),
                    make_record(
                        seq=3,
                        ts=300,
                        payload_tag=analyzer.PAYLOAD_NONE,
                        payload_len=17,
                        payload=b"\x00" * 16,
                    ),
                    make_record(
                        seq=4,
                        ts=400,
                        payload_tag=0x7777,
                        payload_len=4,
                        payload=b"\x00" * 16,
                    ),
                ]
            ]
        )
        try:
            records = analyzer.load_txtrace_records(trace_path)
        finally:
            trace_path.unlink(missing_ok=True)

        self.assertEqual(records[-3]["kind"], "Repair")
        self.assertEqual(records[-3]["category"], "txtrace.repair.bad_magic")
        self.assertEqual(records[-2]["category"], "txtrace.repair.version_mismatch")
        self.assertEqual(records[-1]["category"], "txtrace.repair.payload_len_exceeded")
        self.assertEqual(records[0]["payload_tag"], 0x7777)
        self.assertNotIn("payload", records[0])

    def test_load_rawrecords_sorts_and_preserves_repairs(self) -> None:
        raw = b"".join(
            [
                struct.pack("<H6x", 0) + make_record(hart=0, seq=1, ts=300),
                struct.pack("<H6x", 1)
                + make_record(
                    magic=0xDEAD,
                    hart=1,
                    seq=2,
                    ts=100,
                    payload_tag=analyzer.PAYLOAD_NONE,
                    payload_len=0,
                    payload=b"\x00" * 16,
                ),
                struct.pack("<H6x", 1) + make_record(hart=1, seq=3, ts=200),
            ]
        )
        with NamedTemporaryFile(delete=False) as tmp:
            tmp.write(raw)
            raw_path = Path(tmp.name)
        try:
            records = analyzer.load_rawrecords(raw_path)
        finally:
            raw_path.unlink(missing_ok=True)

        self.assertEqual(records[0]["hart"], 1)
        self.assertEqual(records[0]["ts"], 200)
        self.assertEqual(records[1]["hart"], 0)
        self.assertEqual(records[1]["ts"], 300)
        self.assertEqual(records[2]["kind"], "Repair")
        self.assertEqual(records[2]["category"], "txtrace.repair.bad_magic")

    def test_sql_mode_queries_derived_and_record_views(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify SQL mode")
        records = [
            {"kind": "SpanBegin", "hart": 0, "seq": 1, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "Counter", "hart": 0, "seq": 2, "ts": "150", "payload": {"counter_id": 3, "value": 9}},
            {"kind": "SpanEnd", "hart": 0, "seq": 3, "ts": "300", "span": "0x1", "payload": {}},
        ]
        tables = analyzer.build_derived_tables(records)

        output = analyzer.run_sql_query(
            records,
            tables,
            {7: "debug.span", 3: "debug.counter"},
            "select count(*) as records, (select quantile_disc(dur, 0.50) from spans) as p50 from records",
            "csv",
        ).strip()

        self.assertEqual(output, "records,p50\n3,200")

    def test_python_file_receives_parquet_environment(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify Python mode")
        records = [
            {"kind": "SpanBegin", "hart": 0, "ts": "100", "span": "0x1", "name_id": 7, "payload": {}},
            {"kind": "SpanEnd", "hart": 0, "ts": "300", "span": "0x1", "payload": {}},
        ]
        tables = analyzer.build_derived_tables(records)
        with TemporaryDirectory() as tmpdir:
            script = Path(tmpdir) / "script.py"
            durable = Path(tmpdir) / "tables"
            script.write_text(
                "import os, pathlib\n"
                "spans = pathlib.Path(os.environ['TX_OBSERVE_SPANS_PARQUET'])\n"
                "sched = pathlib.Path(os.environ['TX_OBSERVE_SCHED_INTERVALS_PARQUET'])\n"
                "locks = pathlib.Path(os.environ['TX_OBSERVE_LOCK_ROWS_PARQUET'])\n"
                "print(spans.exists(), sched.exists(), locks.exists(), os.environ['TX_OBSERVE_INPUT'])\n"
            )

            proc = analyzer.run_python_file(script, tables, Path("trace.txtrace"), None, durable)

            self.assertEqual(proc.returncode, 0)
            self.assertEqual(proc.stdout.strip(), "True True True trace.txtrace")
            self.assertTrue((durable / "spans.parquet").exists())

    def test_sql_mode_queries_lock_rows(self) -> None:
        duckdb = shutil.which("duckdb")
        if duckdb is None:
            self.skipTest("duckdb CLI is required to verify SQL mode")
        lock_id = analyzer.fnv1a32("debug.lock.process.fds")
        wait_id = analyzer.fnv1a32("debug.lock.wait_ns")
        tables = analyzer.build_derived_tables(
            [
                {
                    "kind": "Instant",
                    "hart": 0,
                    "ts": "100",
                    "payload_tag": analyzer.PAYLOAD_ARG_VALUE,
                    "parent": str(analyzer.LOCK_TRACK_ID),
                    "name_id": lock_id,
                    "payload": {"key": wait_id, "value0": 25},
                }
            ]
        )

        output = analyzer.run_sql_query(
            [],
            tables,
            {},
            "select lock_id, metric_id, quantile_disc(value, 0.50) as p50, max(value) as max_v from lock_rows group by lock_id, metric_id",
            "csv",
        ).strip()

        self.assertEqual(output, f"lock_id,metric_id,p50,max_v\n{lock_id},{wait_id},25,25")

    def test_action_flags_are_mutually_exclusive(self) -> None:
        parser = analyzer.build_arg_parser()
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                parser.parse_args(["--ndjson", "trace.ndjson", "--sql", "select 1", "--python-file", "script.py"])

    def test_analyze_reports_repair_only_streams(self) -> None:
        records = [
            {"kind": "Repair", "category": "txtrace.repair.bad_magic", "hart": 0, "seq_around": "1", "details": "x"},
            {
                "kind": "Repair",
                "category": "txtrace.repair.version_mismatch",
                "hart": 0,
                "seq_around": "2",
                "details": "y",
            },
        ]

        report = analyzer.analyze(records, {}, top=1)

        self.assertIn("records=2", report)
        self.assertIn("trace_records=0", report)
        self.assertIn("kinds={'Repair': 2}", report)


if __name__ == "__main__":
    unittest.main()
