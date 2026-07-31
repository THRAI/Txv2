"""L4 reader and capture-integrity types for Python observe tooling."""

from __future__ import annotations

import gzip
import heapq
import json
import mmap
from pathlib import Path
from typing import Any

TX_TRACE_MAGIC = 0x5254_5854
SUPPORTED_HEADER_VERSION = 0
RING_HEADER_SIZE = 208
RING_PRODUCER_OFF = 64
RING_CONSUMER_OFF = 128
RING_LOST_OFF = 192
LIVE_RAW_RECORD_HEADER_BYTES = 8


def _is_repair_record(record: dict[str, Any]) -> bool:
    return record.get("kind") == "Repair" or str(record.get("category", "")).startswith(
        "txtrace.repair."
    )


class TraceIntegrity:
    def __init__(
        self,
        input_kind: str,
        complete: bool,
        drained_records: int,
        retained_records: int,
        lost_records: int,
        overwritten_records: int,
        repair_count: int,
    ) -> None:
        self.input_kind = input_kind
        self.complete = complete
        self.drained_records = drained_records
        self.retained_records = retained_records
        self.lost_records = lost_records
        self.overwritten_records = overwritten_records
        self.repair_count = repair_count

    @classmethod
    def from_records(cls, input_kind: str, records: list[dict[str, Any]]) -> "TraceIntegrity":
        repair_count = sum(1 for record in records if _is_repair_record(record))
        return cls(
            input_kind=input_kind,
            complete=repair_count == 0,
            drained_records=len(records),
            retained_records=len(records),
            lost_records=0,
            overwritten_records=0,
            repair_count=repair_count,
        )


class TraceLoadResult:
    def __init__(self, stream: Any) -> None:
        self.stream = stream

    @property
    def records(self) -> list[dict[str, Any]]:
        return self.stream.records

    @property
    def integrity(self) -> TraceIntegrity:
        return self.stream.integrity


from ..l5_canonical import (  # noqa: E402
    MAX_HARTS_DAEMON,
    RECORD_STRUCT,
    SUPPORTED_RECORD_SIZE,
    TraceEventStream,
    decode_record_bytes,
    event_order_key,
    is_repair_record,
)


def _u32(data: bytes | mmap.mmap, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 4], "little")


def _u64(data: bytes | mmap.mmap, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 8], "little")


def load_txtrace_stream(path: Path, *, sort_records: bool = True) -> TraceEventStream:
    runs: list[list[dict[str, Any]]] = []
    total_visible_records = 0
    total_lost_records = 0
    overwritten_records = 0
    with path.open("rb") as file:
        with mmap.mmap(file.fileno(), 0, access=mmap.ACCESS_READ) as data:
            if len(data) < 72:
                raise ValueError("txtrace header is truncated")
            magic = _u32(data, 0)
            version = int.from_bytes(data[4:6], "little")
            record_size = int.from_bytes(data[10:12], "little")
            hart_count = int.from_bytes(data[12:14], "little")
            ring_order = data[14]
            rings_off = _u64(data, 64)
            if magic != TX_TRACE_MAGIC:
                raise ValueError(f"bad txtrace magic 0x{magic:08x}")
            if version != SUPPORTED_HEADER_VERSION:
                raise ValueError(f"unsupported txtrace header version {version}")
            if record_size != SUPPORTED_RECORD_SIZE:
                raise ValueError(f"unsupported txtrace record_size {record_size}")
            if hart_count == 0 or hart_count > MAX_HARTS_DAEMON:
                raise ValueError(f"hart_count {hart_count} is outside [1, {MAX_HARTS_DAEMON}]")
            if ring_order < 2 or ring_order > 24:
                raise ValueError(f"ring_order {ring_order} is outside [2, 24]")
            slot_count = 1 << ring_order
            ring_data_size = RING_HEADER_SIZE + slot_count * record_size
            required = rings_off + hart_count * ring_data_size
            if required > len(data):
                raise ValueError(f"trace file truncated: need {required} bytes, got {len(data)}")

            for h in range(hart_count):
                ring_base = rings_off + h * ring_data_size
                ring = data[ring_base : ring_base + RING_HEADER_SIZE]
                ring_hart = int.from_bytes(ring[0:2], "little")
                producer = _u64(data, ring_base + RING_PRODUCER_OFF)
                consumer = _u64(data, ring_base + RING_CONSUMER_OFF)
                lost = _u64(data, ring_base + RING_LOST_OFF)
                produced_window = max(0, producer - consumer)
                overwritten = max(0, produced_window - slot_count)
                effective_consumer = producer - slot_count if overwritten > 0 else consumer
                total_visible_records += producer - effective_consumer
                total_lost_records += lost
                overwritten_records += overwritten
                slots_base = ring_base + RING_HEADER_SIZE
                run: list[dict[str, Any]] = []
                cursor = effective_consumer
                while cursor != producer:
                    slot_idx = cursor & (slot_count - 1)
                    slot_off = slots_base + slot_idx * record_size
                    record = decode_record_bytes(ring_hart, data[slot_off : slot_off + record_size])
                    if record is not None and record.get("kind") != "Nop":
                        run.append(record)
                    cursor += 1
                run.sort(key=event_order_key)
                runs.append(run)
    records = (
        [record for run in runs for record in run]
        if not sort_records or len(runs) <= 1
        else list(heapq.merge(*runs, key=event_order_key))
    )
    repair_count = sum(1 for record in records if is_repair_record(record))
    integrity = TraceIntegrity(
        input_kind="txtrace",
        complete=total_lost_records == 0 and overwritten_records == 0 and repair_count == 0,
        drained_records=total_visible_records,
        retained_records=len(records),
        lost_records=total_lost_records,
        overwritten_records=overwritten_records,
        repair_count=repair_count,
    )
    return TraceEventStream(records, integrity)


def load_txtrace_records(path: Path, *, sort_records: bool = True) -> list[dict[str, Any]]:
    return load_txtrace_stream(path, sort_records=sort_records).records


def load_rawrecords_stream(path: Path, *, sort_records: bool = True) -> TraceEventStream:
    records: list[dict[str, Any]] = []
    drained_records = 0
    record_bytes = RECORD_STRUCT.size
    stride = LIVE_RAW_RECORD_HEADER_BYTES + record_bytes
    opener = gzip.open if path.suffix == ".gz" else Path.open
    with opener(path, "rb") as file:
        offset = 0
        while True:
            chunk = file.read(stride)
            if not chunk:
                break
            if len(chunk) != stride:
                raise ValueError(
                    f"truncated rawrecords entry at byte {offset}: need {stride} bytes, got {len(chunk)}"
                )
            ring_hart = int.from_bytes(chunk[0:2], "little")
            record = decode_record_bytes(ring_hart, chunk[LIVE_RAW_RECORD_HEADER_BYTES:])
            drained_records += 1
            if record is not None and record.get("kind") != "Nop":
                records.append(record)
            offset += stride
    if sort_records:
        records.sort(key=event_order_key)
    repair_count = sum(1 for record in records if is_repair_record(record))
    integrity = TraceIntegrity(
        input_kind="rawrecords-gzip" if path.suffix == ".gz" else "rawrecords",
        complete=repair_count == 0,
        drained_records=drained_records,
        retained_records=len(records),
        lost_records=0,
        overwritten_records=0,
        repair_count=repair_count,
    )
    return TraceEventStream(records, integrity)


def load_rawrecords(path: Path, *, sort_records: bool = True) -> list[dict[str, Any]]:
    return load_rawrecords_stream(path, sort_records=sort_records).records


def load_records(path: Path, *, sort_records: bool = True) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    last_key: tuple[int, int, int, int] | None = None
    presorted = True
    with path.open() as file:
        for line in file:
            line = line.strip()
            if not line.startswith("{"):
                continue
            record = json.loads(line)
            if "kind" not in record:
                continue
            if record.get("kind") != "Repair" and "ts" not in record:
                continue
            key = event_order_key(record)
            if last_key is not None and key < last_key:
                presorted = False
            last_key = key
            records.append(record)
    if sort_records and not presorted:
        records.sort(key=event_order_key)
    return records


def load_records_stream(path: Path, *, sort_records: bool = True) -> TraceEventStream:
    records = load_records(path, sort_records=sort_records)
    return TraceEventStream(records, TraceIntegrity.from_records("ndjson", records))
