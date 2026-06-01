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
import hashlib
import heapq
import json
import mmap
import os
import re
import shutil
import subprocess
import struct
import sys
import tempfile
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

ANALYZER_DECODER_VERSION = "tx-observe-analyze-derived-v3"
PARQUET_MANIFEST = "_tx_observe_parquet.json"
LOCK_TRACK_ID = 0xD500_0000_0000_000F

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
]

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

TX_TRACE_MAGIC = 0x5254_5854
RECORD_MAGIC = 0x5254
SUPPORTED_HEADER_VERSION = 0
SUPPORTED_RECORD_VERSION = 0
SUPPORTED_RECORD_SIZE = 80
MAX_HARTS_DAEMON = 256
RING_HEADER_SIZE = 208
RING_PRODUCER_OFF = 64
RING_CONSUMER_OFF = 128
RECORD_STRUCT = struct.Struct("<HBBBBBBHHIQQQQIHH16s8s")
LIVE_RAW_RECORD_HEADER_BYTES = 8

KIND_NAMES = {
    0: "Nop",
    1: "ClockSnapshot",
    2: "TrackDescriptor",
    3: "StringDescriptor",
    10: "SpanBegin",
    11: "SpanEnd",
    12: "Instant",
    13: "Counter",
    14: "TrackTombstone",
    31: "PanicMarker",
    40: "ArgContinuation",
}

LEVEL_NAMES = {
    0: "Boundary",
    1: "Script",
    2: "Drive",
    3: "Yield",
    4: "Step",
    5: "Phase",
    6: "Mutation",
    7: "Sched",
}

PAYLOAD_NONE = 0
PAYLOAD_SYSCALL_ENTER = 1
PAYLOAD_SYSCALL_EXIT = 2
PAYLOAD_DRIVE_BEGIN = 10
PAYLOAD_DRIVE_END = 11
PAYLOAD_STEP_OUTCOME = 12
PAYLOAD_YIELD_BEGIN = 20
PAYLOAD_RESUME = 21
PAYLOAD_WAIT_SOURCE_NOTIFY = 22
PAYLOAD_AGENT_STATE_CHANGE = 23
PAYLOAD_TRACK_DESCRIPTOR = 30
PAYLOAD_COUNTER_VALUE = 31
PAYLOAD_STRING_DESCRIPTOR = 32
PAYLOAD_CLOCK_SNAPSHOT = 33
PAYLOAD_ARG_VALUE = 40
PAYLOAD_MUTATION_ZONE_SIGN = 50
PAYLOAD_MUTATION_INDEX_COMMIT = 51
PAYLOAD_PHASE_TRANSITION = 52
PAYLOAD_SCHED_SWITCH = 53
PAYLOAD_PROCESS_LABEL = 54
PAYLOAD_PROCESS_GROUP = 55
PAYLOAD_PROCESS_FORK = 56
PAYLOAD_PANIC = 60

PAYLOAD_MIN_LENGTHS = {
    PAYLOAD_SYSCALL_ENTER: 8,
    PAYLOAD_SYSCALL_EXIT: 16,
    PAYLOAD_DRIVE_BEGIN: 12,
    PAYLOAD_DRIVE_END: 16,
    PAYLOAD_STEP_OUTCOME: 16,
    PAYLOAD_YIELD_BEGIN: 16,
    PAYLOAD_RESUME: 16,
    PAYLOAD_WAIT_SOURCE_NOTIFY: 16,
    PAYLOAD_TRACK_DESCRIPTOR: 16,
    PAYLOAD_COUNTER_VALUE: 16,
    PAYLOAD_CLOCK_SNAPSHOT: 16,
    PAYLOAD_ARG_VALUE: 16,
    PAYLOAD_MUTATION_ZONE_SIGN: 16,
    PAYLOAD_MUTATION_INDEX_COMMIT: 16,
    PAYLOAD_PHASE_TRANSITION: 16,
    PAYLOAD_SCHED_SWITCH: 16,
    PAYLOAD_PROCESS_LABEL: 16,
    PAYLOAD_PROCESS_GROUP: 16,
    PAYLOAD_PROCESS_FORK: 16,
    PAYLOAD_PANIC: 16,
}


def fnv1a32(text: str) -> int:
    value = 0x811C9DC5
    for byte in text.encode():
        value ^= byte
        value = (value * 0x01000193) & 0xFFFFFFFF
    return value


def parse_u32_id(value: Any) -> int | None:
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value, 16) if value.startswith("0x") else int(value)
        except ValueError:
            return None
    return None


def parse_u64_id(value: Any) -> int | None:
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value, 16) if value.startswith("0x") else int(value)
        except ValueError:
            return None
    return None


def record_ts(record: dict[str, Any]) -> int:
    ts = parse_u64_id(record.get("ts"))
    if ts is None:
        raise ValueError(f"record has invalid timestamp: {record.get('ts')!r}")
    return ts


def record_hart(record: dict[str, Any]) -> int | None:
    return parse_u32_id(record.get("hart"))


def is_trace_record(record: dict[str, Any]) -> bool:
    return record.get("kind") != "Repair" and parse_u64_id(record.get("ts")) is not None


def timeline_records(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [record for record in records if is_trace_record(record)]


def payload_u64(payload: dict[str, Any], key: str) -> int | None:
    return parse_u64_id(payload.get(key))


def record_order_key(record: dict[str, Any]) -> tuple[int, int, int]:
    return (
        record_ts(record),
        record_hart(record) or 0,
        parse_u64_id(record.get("seq")) or 0,
    )


def event_order_key(record: dict[str, Any]) -> tuple[int, int, int, int]:
    if record.get("kind") == "Repair":
        return (
            (1 << 64) - 1,
            parse_u32_id(record.get("hart")) or 0,
            parse_u64_id(record.get("seq_around")) or 0,
            1,
        )
    return (*record_order_key(record), 0)


def _u32(data: bytes | mmap.mmap, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 4], "little")


def _u64(data: bytes | mmap.mmap, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 8], "little")


def _decode_comm(raw: bytes) -> list[int]:
    return list(raw[:12])


def repair_record(category: str, hart: int, seq_around: int, details: str) -> dict[str, Any]:
    return {
        "kind": "Repair",
        "category": category,
        "hart": hart,
        "seq_around": seq_around,
        "details": details,
    }


def decode_payload_bytes(
    ring_hart: int,
    seq_around: int,
    tag: int,
    payload_len: int,
    payload: bytes,
) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    if tag == PAYLOAD_NONE:
        return None, None
    if payload_len > 16:
        return None, repair_record(
            "txtrace.repair.payload_len_exceeded",
            ring_hart,
            seq_around,
            f"payload_len {payload_len} exceeds 16-byte inline buffer",
        )
    if tag in {PAYLOAD_AGENT_STATE_CHANGE, PAYLOAD_STRING_DESCRIPTOR}:
        return None, None
    required_len = PAYLOAD_MIN_LENGTHS.get(tag)
    if required_len is None:
        return None, None
    if payload_len < required_len:
        return None, repair_record(
            "txtrace.repair.payload_tag_unknown",
            ring_hart,
            seq_around,
            f"short payload for payload_tag 0x{tag:04x}: need {required_len} bytes, got {payload_len}",
        )
    if tag == PAYLOAD_SYSCALL_ENTER and payload_len >= 8:
        sysno, abi, argc = struct.unpack_from("<IHH", payload)
        return {"sysno": sysno, "abi": abi, "argc": argc}, None
    if tag == PAYLOAD_SYSCALL_EXIT and payload_len >= 16:
        ret, errno, result_kind = struct.unpack_from("<qiB", payload)
        return {"ret": ret, "errno": errno, "result_kind": result_kind, "_pad": list(payload[13:16])}, None
    if tag == PAYLOAD_DRIVE_BEGIN and payload_len >= 12:
        op_type, mode, interrupt, has_deadline, _pad, task_id_low = struct.unpack_from("<IBBBBI", payload)
        return {
            "op_type": op_type,
            "mode": mode,
            "interrupt": interrupt,
            "has_deadline": has_deadline,
            "_pad": _pad,
            "task_id_low": task_id_low,
        }, None
    if tag == PAYLOAD_DRIVE_END and payload_len >= 16:
        ret, errno, result_kind = struct.unpack_from("<qiB", payload)
        return {"ret": ret, "errno": errno, "result_kind": result_kind, "_pad": list(payload[13:16])}, None
    if tag == PAYLOAD_STEP_OUTCOME and payload_len >= 16:
        variant, progress_empty, progress_kind, shape_kind, errno, progress_value, _pad = struct.unpack_from(
            "<BBBBiII", payload
        )
        return {
            "variant": variant,
            "progress_empty": progress_empty,
            "progress_kind": progress_kind,
            "shape_kind": shape_kind,
            "errno": errno,
            "progress_value": progress_value,
            "_pad": _pad,
        }, None
    if tag == PAYLOAD_YIELD_BEGIN and payload_len >= 16:
        shape_kind = payload[0]
        task_id_low = struct.unpack_from("<I", payload, 4)[0]
        wait_generation = struct.unpack_from("<Q", payload, 8)[0]
        return {
            "shape_kind": shape_kind,
            "_pad": list(payload[1:4]),
            "task_id_low": task_id_low,
            "wait_generation": wait_generation,
        }, None
    if tag == PAYLOAD_RESUME and payload_len >= 16:
        resume_kind, abort_reason = struct.unpack_from("<BB", payload)
        object_id_low = struct.unpack_from("<I", payload, 4)[0]
        wait_generation = struct.unpack_from("<Q", payload, 8)[0]
        return {
            "resume_kind": resume_kind,
            "abort_reason": abort_reason,
            "_pad": list(payload[2:4]),
            "object_id_low": object_id_low,
            "wait_generation": wait_generation,
        }, None
    if tag == PAYLOAD_WAIT_SOURCE_NOTIFY and payload_len >= 16:
        source_id_low, mask_bits, task_id_low, wait_generation_low = struct.unpack_from("<IIII", payload)
        return {
            "source_id_low": source_id_low,
            "mask_bits": mask_bits,
            "task_id_low": task_id_low,
            "wait_generation_low": wait_generation_low,
        }, None
    if tag == PAYLOAD_TRACK_DESCRIPTOR and payload_len >= 16:
        track_id, name, track_kind = struct.unpack_from("<QIB", payload)
        return {"track_id": track_id, "name": name, "track_kind": track_kind, "_pad": list(payload[13:16])}, None
    if tag == PAYLOAD_COUNTER_VALUE and payload_len >= 16:
        counter_id, _pad, value = struct.unpack_from("<IIQ", payload)
        return {"counter_id": counter_id, "_pad": _pad, "value": value}, None
    if tag == PAYLOAD_CLOCK_SNAPSHOT and payload_len >= 16:
        trace_ns, wall_ns = struct.unpack_from("<QQ", payload)
        return {"trace_ns": trace_ns, "wall_ns": wall_ns}, None
    if tag == PAYLOAD_ARG_VALUE and payload_len >= 16:
        key = struct.unpack_from("<I", payload)[0]
        value_kind = payload[4]
        value0 = struct.unpack_from("<Q", payload, 8)[0]
        return {"key": key, "value_kind": value_kind, "_pad": list(payload[5:8]), "value0": value0}, None
    if tag == PAYLOAD_MUTATION_ZONE_SIGN and payload_len >= 16:
        object_id = struct.unpack_from("<Q", payload)[0]
        kind = payload[8]
        return {"object_id": object_id, "kind": kind, "_pad": list(payload[9:16])}, None
    if tag == PAYLOAD_MUTATION_INDEX_COMMIT and payload_len >= 16:
        index_id, key_low, value_object_id = struct.unpack_from("<IIQ", payload)
        return {"index_id": index_id, "key_low": key_low, "value_object_id": value_object_id}, None
    if tag == PAYLOAD_PHASE_TRANSITION and payload_len >= 16:
        return {"phase_kind": payload[0], "hart_id": payload[1], "_pad": list(payload[2:16])}, None
    if tag == PAYLOAD_SCHED_SWITCH and payload_len >= 16:
        task_id_low, process_id_low, hart_id, kind, reason = struct.unpack_from("<IIBBB", payload)
        return {
            "task_id_low": task_id_low,
            "process_id_low": process_id_low,
            "hart_id": hart_id,
            "kind": kind,
            "reason": reason,
            "_pad": list(payload[11:16]),
        }, None
    if tag == PAYLOAD_PROCESS_LABEL and payload_len >= 16:
        process_id_low = struct.unpack_from("<I", payload)[0]
        return {"process_id_low": process_id_low, "comm": _decode_comm(payload[4:16])}, None
    if tag == PAYLOAD_PROCESS_GROUP and payload_len >= 16:
        process_id_low, pgid_low, sid_low, _pad = struct.unpack_from("<IIII", payload)
        return {"process_id_low": process_id_low, "pgid_low": pgid_low, "sid_low": sid_low, "_pad": _pad}, None
    if tag == PAYLOAD_PROCESS_FORK and payload_len >= 16:
        parent_pid_low, child_pid_low, flags, _pad = struct.unpack_from("<IIII", payload)
        return {"parent_pid_low": parent_pid_low, "child_pid_low": child_pid_low, "_flags": flags, "_pad": _pad}, None
    if tag == PAYLOAD_PANIC and payload_len >= 16:
        site_name, _pad, panic_hart, flags, _pad2 = struct.unpack_from("<IIHHI", payload)
        return {"site_name": site_name, "_pad": _pad, "panic_hart": panic_hart, "flags": flags, "_pad2": _pad2}, None
    return None, None


def decode_record_bytes(ring_hart: int, raw: bytes) -> dict[str, Any] | None:
    (
        magic,
        version,
        kind,
        level,
        _flags,
        _arg_count,
        _pad0,
        hart,
        _pad1,
        _pad2,
        seq,
        ts,
        span,
        parent,
        name,
        payload_tag,
        payload_len,
        payload,
        _pad3,
    ) = RECORD_STRUCT.unpack(raw)
    if magic != RECORD_MAGIC:
        return repair_record(
            "txtrace.repair.bad_magic",
            ring_hart,
            seq,
            f"expected 0x{RECORD_MAGIC:04x} got 0x{magic:04x}",
        )
    if version != SUPPORTED_RECORD_VERSION:
        return repair_record(
            "txtrace.repair.version_mismatch",
            ring_hart,
            seq,
            f"expected version {SUPPORTED_RECORD_VERSION} got {version}",
        )
    if payload_len > 16:
        return repair_record(
            "txtrace.repair.payload_len_exceeded",
            ring_hart,
            seq,
            f"payload_len {payload_len} exceeds 16-byte inline buffer",
        )
    record: dict[str, Any] = {
        "hart": hart if hart < MAX_HARTS_DAEMON else ring_hart,
        "seq": seq,
        "ts": ts,
        "kind": KIND_NAMES.get(kind, "Unknown"),
        "level": LEVEL_NAMES.get(level, "Unknown"),
        "span": f"0x{span:x}",
        "parent": f"0x{parent:x}",
        "name_id": f"0x{name:x}",
        "payload_tag": payload_tag,
    }
    decoded_payload, repair = decode_payload_bytes(ring_hart, seq, payload_tag, payload_len, payload)
    if repair is not None:
        return repair
    if decoded_payload is not None:
        record["payload"] = decoded_payload
    return record


def load_txtrace_records(path: Path, *, sort_records: bool = True) -> list[dict[str, Any]]:
    runs: list[list[dict[str, Any]]] = []
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
                effective_consumer = (
                    producer - slot_count if producer - consumer > slot_count else consumer
                )
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
    if not sort_records or len(runs) <= 1:
        return [record for run in runs for record in run]
    return list(heapq.merge(*runs, key=event_order_key))


def load_rawrecords(path: Path, *, sort_records: bool = True) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    record_bytes = RECORD_STRUCT.size
    stride = LIVE_RAW_RECORD_HEADER_BYTES + record_bytes
    with path.open("rb") as file:
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
            if record is not None and record.get("kind") != "Nop":
                records.append(record)
            offset += stride
    if sort_records:
        records.sort(key=event_order_key)
    return records


def fmt_ns(ns: float) -> str:
    return f"{ns / 1000.0:.1f}us"


def fmt_counter_value(name: str, value: int) -> str:
    if (
        name.endswith(".uaddr")
        or name.endswith(".target_uaddr")
        or name.endswith(".mask")
        or name.endswith(".fired_mask")
        or name.endswith(".source")
        or name.endswith(".range_start")
        or name.endswith(".reclaim_fn")
        or name.endswith(".fn")
    ):
        return hex(value)
    return str(value)


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
    ) -> None:
        self.spans = spans
        self.counters = counters
        self.allocation_rows = allocation_rows
        self.sched_intervals = sched_intervals or []
        self.lock_rows = lock_rows or []
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
    }


def derived_tables_from_json(data: dict[str, Any]) -> DerivedTables:
    return DerivedTables(
        spans=list(data.get("spans") or []),
        counters=list(data.get("counters") or []),
        allocation_rows=list(data.get("allocation_rows") or []),
        sched_intervals=list(data.get("sched_intervals") or []),
        lock_rows=list(data.get("lock_rows") or []),
        meta=dict(data.get("meta") or {}),
    )


PARQUET_SCHEMAS = {
    "spans.parquet": [
        ("span", "VARCHAR"),
        ("name_id", "UINTEGER"),
        ("sysno", "UBIGINT"),
        ("begin", "UBIGINT"),
        ("end", "UBIGINT"),
        ("dur", "UBIGINT"),
        ("begin_hart", "UINTEGER"),
        ("end_hart", "UINTEGER"),
        ("task_id", "UBIGINT"),
        ("process_id", "UBIGINT"),
        ("ret", "BIGINT"),
        ("errno", "BIGINT"),
        ("dangling", "BOOLEAN"),
        ("sched_harts", "UINTEGER[]"),
        ("sched_segments", "UBIGINT"),
        ("sched_coverage", "UBIGINT"),
        ("sched_migrated", "BOOLEAN"),
    ],
    "counters.parquet": [
        ("ts", "UBIGINT"),
        ("counter_id", "UINTEGER"),
        ("value", "UBIGINT"),
    ],
    "allocation_rows.parquet": [
        ("ts", "UBIGINT"),
        ("track", "VARCHAR"),
        ("name_id", "UINTEGER"),
        ("value", "UBIGINT"),
    ],
    "sched_intervals.parquet": [
        ("task", "UBIGINT"),
        ("pid", "UBIGINT"),
        ("hart", "UINTEGER"),
        ("begin", "UBIGINT"),
        ("end", "UBIGINT"),
        ("dur", "UBIGINT"),
    ],
    "lock_rows.parquet": [
        ("ts", "UBIGINT"),
        ("hart", "UINTEGER"),
        ("lock_id", "UINTEGER"),
        ("metric_id", "UINTEGER"),
        ("value", "UBIGINT"),
    ],
}


def duckdb_sql_string(value: Path | str) -> str:
    text = value.as_posix() if isinstance(value, Path) else value
    return "'" + text.replace("'", "''") + "'"


def parquet_select_sql(
    filename: str,
    json_path: Path,
    rows: list[dict[str, Any]],
) -> str:
    schema = PARQUET_SCHEMAS[filename]
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


def duckdb_json_columns(schema: list[tuple[str, str]]) -> str:
    fields = ", ".join(f"'{column}':'VARCHAR'" for column, _ in schema)
    return "{" + fields + "}"


def read_json_typed(path: Path, schema: list[tuple[str, str]]) -> str:
    return (
        "read_json("
        + duckdb_sql_string(path)
        + f", columns={duckdb_json_columns(schema)}, format='newline_delimited')"
    )


RECORD_SQL_SCHEMA = [
    ("ts", "UBIGINT"),
    ("hart", "UINTEGER"),
    ("seq", "UBIGINT"),
    ("kind", "VARCHAR"),
    ("level", "VARCHAR"),
    ("span", "VARCHAR"),
    ("parent", "VARCHAR"),
    ("name_id", "UINTEGER"),
    ("payload_tag", "UINTEGER"),
]

REPAIR_SQL_SCHEMA = [
    ("hart", "UINTEGER"),
    ("seq_around", "UBIGINT"),
    ("category", "VARCHAR"),
    ("details", "VARCHAR"),
]

NAME_SQL_SCHEMA = [
    ("id", "UINTEGER"),
    ("name", "VARCHAR"),
]


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
    expected = set(PARQUET_SCHEMAS)
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
        "files": sorted(PARQUET_SCHEMAS),
        "table_counts": {
            "spans": len(tables.spans),
            "counters": len(tables.counters),
            "allocation_rows": len(tables.allocation_rows),
            "sched_intervals": len(tables.sched_intervals),
            "lock_rows": len(tables.lock_rows),
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
    written: list[Path] = []
    exports = [
        (
            "spans.parquet",
            tables.spans,
            [column for column, _ in PARQUET_SCHEMAS["spans.parquet"]],
        ),
        ("counters.parquet", tables.counters, [column for column, _ in PARQUET_SCHEMAS["counters.parquet"]]),
        (
            "allocation_rows.parquet",
            tables.allocation_rows,
            [column for column, _ in PARQUET_SCHEMAS["allocation_rows.parquet"]],
        ),
        (
            "sched_intervals.parquet",
            tables.sched_intervals,
            [column for column, _ in PARQUET_SCHEMAS["sched_intervals.parquet"]],
        ),
        (
            "lock_rows.parquet",
            tables.lock_rows,
            [column for column, _ in PARQUET_SCHEMAS["lock_rows.parquet"]],
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
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        table_defs = [
            ("records", sql_records_rows(records), RECORD_SQL_SCHEMA),
            ("repairs", sql_repairs_rows(records), REPAIR_SQL_SCHEMA),
            ("spans", tables.spans, PARQUET_SCHEMAS["spans.parquet"]),
            ("counters", tables.counters, PARQUET_SCHEMAS["counters.parquet"]),
            ("allocation_rows", tables.allocation_rows, PARQUET_SCHEMAS["allocation_rows.parquet"]),
            ("sched_intervals", tables.sched_intervals, PARQUET_SCHEMAS["sched_intervals.parquet"]),
            ("lock_rows", tables.lock_rows, PARQUET_SCHEMAS["lock_rows.parquet"]),
            ("names", sql_names_rows(names), NAME_SQL_SCHEMA),
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
            f"lock_rows={counts.get('lock_rows', 0)}"
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


def input_arg_path(args: argparse.Namespace) -> Path:
    if args.ndjson is not None:
        return args.ndjson
    if args.rawrecords is not None:
        return args.rawrecords
    return args.file


def load_input_records(args: argparse.Namespace) -> list[dict[str, Any]]:
    if args.ndjson is not None:
        return load_records(args.ndjson, sort_records=not args.no_sort)
    if args.rawrecords is not None:
        return load_rawrecords(args.rawrecords, sort_records=not args.no_sort)
    return load_txtrace_records(args.file, sort_records=not args.no_sort)


def analyze(
    records: list[dict[str, Any]],
    names: dict[int, str],
    top: int,
    derived: DerivedTables | None = None,
) -> str:
    derived = derived or build_derived_tables(records)
    spans = derived.spans
    counters = derived.counters
    span_by_id = {span["span"]: span for span in spans}
    kinds = Counter(record.get("kind") for record in records)
    trace_records = timeline_records(records)

    out: list[str] = []
    if trace_records:
        out.append(
            f"records={len(records)} trace_records={len(trace_records)} "
            f"window={fmt_ns(record_ts(trace_records[-1]) - record_ts(trace_records[0]))} "
            f"kinds={dict(kinds)} unclosed_spans={derived.meta.get('dangling_span_count', 0)} "
            f"dangling_policy={derived.meta.get('dangling_span_policy', 'exclude')} "
            f"dangling_materialized={derived.meta.get('dangling_spans_materialized', 0)}"
        )
    else:
        out.append(
            f"records={len(records)} trace_records=0 kinds={dict(kinds)} "
            f"unclosed_spans={derived.meta.get('dangling_span_count', 0)} "
            f"dangling_policy={derived.meta.get('dangling_span_policy', 'exclude')} "
            f"dangling_materialized={derived.meta.get('dangling_spans_materialized', 0)}"
        )

    grouped: dict[str, dict[str, Any]] = {}
    for span in spans:
        name = span_display_name(span, names)
        key = f"{span.get('sysno')}:{name}"
        row = grouped.setdefault(
            key,
            {
                "name": name,
                "sysno": span.get("sysno"),
                "n": 0,
                "sum": 0,
                "min": None,
                "max": 0,
            },
        )
        row["n"] += 1
        row["sum"] += span["dur"]
        row["max"] = max(row["max"], span["dur"])
        row["min"] = span["dur"] if row["min"] is None else min(row["min"], span["dur"])

    out.append("")
    out.append("by span total:")
    for row in sorted(grouped.values(), key=lambda r: r["sum"], reverse=True)[:top]:
        avg = row["sum"] / row["n"]
        out.append(
            f"{str(row['sysno']).rjust(4)} {row['name'][:32].ljust(32)} "
            f"n={row['n']:>5} total={fmt_ns(row['sum']):>11} "
            f"avg={fmt_ns(avg):>10} max={fmt_ns(row['max']):>10} "
            f"min={fmt_ns(row['min']):>10}"
        )

    out.append("")
    out.append("slowest spans:")
    for span in sorted(spans, key=lambda s: s["dur"], reverse=True)[:top]:
        begin_hart = span.get("begin_hart")
        end_hart = span.get("end_hart")
        migration = (
            ""
            if begin_hart is None or end_hart is None
            else f" harts={begin_hart}->{end_hart} net_migrated={str(begin_hart != end_hart).lower()}"
        )
        sched_harts = span.get("sched_harts")
        if isinstance(sched_harts, list) and sched_harts:
            migration += (
                " sched_harts="
                + "->".join(str(hart) for hart in sched_harts)
                + f" sched_migrated={str(bool(span.get('sched_migrated'))).lower()}"
            )
        dangling = " dangling=true" if span.get("dangling") else ""
        name = span_display_name(span, names)
        out.append(
            f"{span['span']} {span.get('sysno')}:{name} "
            f"dur={fmt_ns(span['dur'])}{migration}{dangling} ret={span.get('ret')} errno={span.get('errno')}"
        )

    out.append("")
    out.append("largest inter-record gaps:")
    prev_by_hart: dict[int | None, dict[str, Any]] = {}
    gaps: list[tuple[int, int | None, dict[str, Any], dict[str, Any]]] = []
    for record in trace_records:
        hart = record_hart(record)
        prev = prev_by_hart.get(hart)
        if prev is not None:
            gaps.append((record_ts(record) - record_ts(prev), hart, prev, record))
        prev_by_hart[hart] = record
    for delta, hart, prev, cur in heapq.nlargest(top, gaps, key=lambda g: g[0]):
        hart_label = "?" if hart is None else str(hart)
        out.append(
            f"hart={hart_label} dt={fmt_ns(delta)} {describe(prev, span_by_id, names)} -> "
            f"{describe(cur, span_by_id, names)}"
        )

    hist: dict[int, Counter[int]] = defaultdict(Counter)
    for row in counters:
        hist[row["counter_id"]][row["value"]] += 1
    if hist:
        out.append("")
        out.append("counter histograms:")
        for cid, counter in sorted(hist.items(), key=lambda item: sum(item[1].values()), reverse=True)[:top]:
            name = names.get(cid, f"counter_0x{cid:x}")
            values = " ".join(f"{value}:{count}" for value, count in counter.most_common(12))
            out.append(f"{name}: {values}")

    return "\n".join(out)


def percentile(values: list[int], pct: float) -> int:
    if not values:
        return 0
    idx = int((len(values) - 1) * pct)
    return sorted(values)[idx]


def roundtrip_points(
    records: list[dict[str, Any]], names: dict[int, str], sysno: int
) -> list[tuple[int, str]]:
    points: list[tuple[int, str]] = []
    span_names: dict[str, str] = {}
    for record in records:
        ts = record_ts(record)
        kind = record.get("kind")
        span_id = record.get("span")
        payload = record.get("payload") or {}
        if kind == "Counter":
            cid = payload.get("counter_id")
            val = payload_u64(payload, "value")
            if isinstance(cid, int) and val == sysno:
                name = names.get(cid, f"counter_0x{cid:x}")
                if name.startswith("debug."):
                    points.append((ts, name))
        elif kind == "SpanBegin":
            name = event_name(record, names)
            if payload.get("sysno") == sysno:
                if isinstance(span_id, str):
                    span_names[span_id] = name
                points.append((ts, f"{name}.begin"))
        elif kind == "SpanEnd" and isinstance(span_id, str):
            name = span_names.get(span_id)
            if name is not None:
                points.append((ts, f"{name}.end"))
    points.sort()
    return points


def analyze_roundtrip(
    records: list[dict[str, Any]], names: dict[int, str], sysno: int, top: int
) -> str:
    points = roundtrip_points(records, names, sysno)
    groups: dict[str, list[int]] = defaultdict(list)
    windows = 0
    window: list[tuple[int, str]] = []

    def flush(next_start: tuple[int, str] | None = None) -> None:
        nonlocal windows
        if len(window) < 2:
            return
        if next_start is not None:
            prev_ts, prev_name = window[-1]
            cur_ts, cur_name = next_start
            groups[f"{prev_name} -> next {cur_name}"].append(cur_ts - prev_ts)
        windows += 1
        for (prev_ts, prev_name), (cur_ts, cur_name) in zip(window, window[1:]):
            groups[f"{prev_name} -> {cur_name}"].append(cur_ts - prev_ts)

    for point in points:
        _, name = point
        if name == "debug.trap.syscall":
            flush(point)
            window = [point]
        elif window:
            window.append(point)
    flush()

    out = ["", f"roundtrip sysno={sysno}: windows={windows} points={len(points)}"]
    rows = sorted(groups.items(), key=lambda item: sum(item[1]), reverse=True)
    for name, values in rows[:top]:
        values_sorted = sorted(values)
        total = sum(values_sorted)
        avg = total / len(values_sorted)
        out.append(
            f"{name[:72].ljust(72)} n={len(values_sorted):>5} "
            f"avg={fmt_ns(avg):>10} p50={fmt_ns(percentile(values_sorted, 0.50)):>10} "
            f"p95={fmt_ns(percentile(values_sorted, 0.95)):>10} "
            f"max={fmt_ns(values_sorted[-1]):>10}"
        )
    return "\n".join(out)


CLONE_THREAD_PHASES = [
    "debug.clone_thread.enter",
    "debug.clone_thread.allocate_tid.after",
    "debug.clone_thread.payload_fresh.after",
    "debug.clone_thread.payload_sign.after",
    "debug.clone_thread.payload_cap.after",
    "debug.clone_thread.identity_sign.after",
    "debug.clone_thread.sign_thread.after",
    "debug.clone_thread.register_tid.after",
    "debug.clone_thread.seed_context.after",
    "debug.clone_thread.clear_ctid.after",
    "debug.clone_thread.attach.after",
]

CHILD_SUBMIT_PHASES = [
    "debug.child_submit.enter",
    "debug.child_submit.payload.after",
    "debug.child_submit.payload_clone.after",
    "debug.child_submit.reactor.with.before",
    "debug.child_submit.submit_call.before",
    "debug.child_submit.reactor.with.after",
    "debug.child_submit.publish.queue",
    "debug.child_submit.publish.hart",
    "debug.child_submit.publish.dispatch",
    "debug.child_submit.register.after",
]

TASK_SUBMIT_PHASES = [
    "debug.task.submit.slot.after",
    "debug.task.submit.future_size",
    "debug.task.submit.future_box.after",
    "debug.task.submit.wake_state.after",
    "debug.task.submit.mailbox.after",
    "debug.task.submit.construct.after",
    "debug.task.submit.store.after",
]

THREAD_EXIT_PHASES = [
    "debug.thread_exit.enter",
    "debug.thread_exit.snapshot.after",
    "debug.thread_exit.zombie.after",
    "debug.thread_exit.parent.after",
    "debug.thread_exit.retain.after",
    "debug.thread_exit.count.after",
    "debug.thread_exit.ctid.after",
    "debug.thread_exit.robust.after",
    "debug.thread_exit.end",
]


def analyze_clone_thread_phases(
    records: list[dict[str, Any]], names: dict[int, str], top: int
) -> str:
    per_tid: dict[int, dict[str, int]] = defaultdict(dict)
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        tid = payload_u64(payload, "value")
        if not isinstance(cid, int) or tid is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name not in CLONE_THREAD_PHASES:
            continue
        per_tid[tid][name] = record_ts(record)

    if not per_tid:
        return "\nclone_thread phases: none"

    groups: dict[str, list[tuple[int, int]]] = defaultdict(list)
    complete: list[tuple[int, int]] = []
    for tid, seen in per_tid.items():
        if all(phase in seen for phase in CLONE_THREAD_PHASES[1:]):
            start = seen[CLONE_THREAD_PHASES[1]]
            end = seen[CLONE_THREAD_PHASES[-1]]
            complete.append((end - start, tid))
        for prev, cur in zip(CLONE_THREAD_PHASES, CLONE_THREAD_PHASES[1:]):
            if prev in seen and cur in seen:
                groups[f"{prev} -> {cur}"].append((seen[cur] - seen[prev], tid))

    out = ["", f"clone_thread phases: tids={len(per_tid)} complete={len(complete)}"]
    if complete:
        values = sorted(delta for delta, _tid in complete)
        out.append(
            f"allocate_tid.after -> attach.after total "
            f"avg={fmt_ns(sum(values)/len(values))} "
            f"p50={fmt_ns(percentile(values, 0.50))} "
            f"p95={fmt_ns(percentile(values, 0.95))} "
            f"max={fmt_ns(values[-1])}"
        )
    for name, rows in sorted(groups.items(), key=lambda item: sum(delta for delta, _tid in item[1]), reverse=True):
        values = sorted(delta for delta, _tid in rows)
        out.append(
            f"{name[:72].ljust(72)} n={len(values):>5} "
            f"avg={fmt_ns(sum(values)/len(values)):>10} "
            f"p50={fmt_ns(percentile(values, 0.50)):>10} "
            f"p95={fmt_ns(percentile(values, 0.95)):>10} "
            f"max={fmt_ns(values[-1]):>10}"
        )

    slow = sorted(
        (
            (delta, tid, name)
            for name, rows in groups.items()
            for delta, tid in rows
        ),
        reverse=True,
    )[:top]
    if slow:
        out.append("slowest clone_thread phase instances:")
        for delta, tid, name in slow:
            out.append(f"tid={tid:<4} {name} dur={fmt_ns(delta)}")
    return "\n".join(out)


def analyze_counter_phase_sequence(
    title: str,
    records: list[dict[str, Any]],
    names: dict[int, str],
    phases: list[str],
    top: int,
    *,
    key_by_value: bool,
) -> str:
    per_key: dict[int, dict[str, int]] = defaultdict(dict)
    seq = 0
    current_key: int | None = None

    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name not in phases:
            continue
        if key_by_value:
            key = value
        else:
            if name == phases[0]:
                seq += 1
                current_key = seq
            if current_key is None:
                continue
            key = current_key
        per_key[key][name] = record_ts(record)

    if not per_key:
        return f"\n{title} phases: none"

    groups: dict[str, list[tuple[int, int]]] = defaultdict(list)
    complete: list[tuple[int, int]] = []
    for key, seen in per_key.items():
        if phases[0] in seen and phases[-1] in seen:
            complete.append((seen[phases[-1]] - seen[phases[0]], key))
        for prev, cur in zip(phases, phases[1:]):
            if prev in seen and cur in seen:
                groups[f"{prev} -> {cur}"].append((seen[cur] - seen[prev], key))

    out = ["", f"{title} phases: keys={len(per_key)} complete={len(complete)}"]
    if complete:
        values = sorted(delta for delta, _key in complete)
        out.append(
            f"{phases[0]} -> {phases[-1]} total "
            f"avg={fmt_ns(sum(values)/len(values))} "
            f"p50={fmt_ns(percentile(values, 0.50))} "
            f"p95={fmt_ns(percentile(values, 0.95))} "
            f"max={fmt_ns(values[-1])}"
        )

    for name, rows in sorted(
        groups.items(), key=lambda item: sum(delta for delta, _key in item[1]), reverse=True
    ):
        values = sorted(delta for delta, _key in rows)
        out.append(
            f"{name[:72].ljust(72)} n={len(values):>5} "
            f"avg={fmt_ns(sum(values)/len(values)):>10} "
            f"p50={fmt_ns(percentile(values, 0.50)):>10} "
            f"p95={fmt_ns(percentile(values, 0.95)):>10} "
            f"max={fmt_ns(values[-1]):>10}"
        )

    slow = sorted(
        (
            (delta, key, name)
            for name, rows in groups.items()
            for delta, key in rows
        ),
        reverse=True,
    )[:top]
    if slow:
        out.append(f"slowest {title} phase instances:")
        for delta, key, name in slow:
            out.append(f"key={key:<4} {name} dur={fmt_ns(delta)}")
    return "\n".join(out)


ARG_NAMES = {
    0x1B24B714: "a0",
    0x1C24B8A7: "a1",
    0x1D24BA3A: "a2",
    0x1E24BBCD: "a3",
    0x1724B0C8: "a4",
    0x1824B25B: "a5",
}

FUTEX_OPS = {
    0: "WAIT",
    1: "WAKE",
    2: "FD",
    3: "REQUEUE",
    4: "CMP_REQUEUE",
    5: "WAKE_OP",
    6: "LOCK_PI",
    7: "UNLOCK_PI",
    8: "TRYLOCK_PI",
    9: "WAIT_BITSET",
    10: "WAKE_BITSET",
}

QUEUE_NAMES = {
    1: "Kernel",
    2: "Boosted",
    3: "New",
    4: "Preempted",
}

STOP_REASON_NAMES = {
    1: "Blocked",
    2: "Completed",
    3: "Yielded",
    4: "SliceExpired",
    5: "UserspaceTrap",
    6: "PreemptedExternal",
}

MAILBOX_HINT_NAMES = {
    0: "Normal",
    1: "WakeHandoff",
    2: "LifecycleWake",
    3: "PriorityBoost",
    4: "SignalDelivery",
}

WAKE_HINT_NAMES = {
    0: "Normal",
    1: "WakeHandoff",
    2: "LifecycleWake",
    3: "PriorityBoost",
    4: "SignalDelivery",
    5: "None",
}


def decode_task_code(value: int) -> tuple[int, int]:
    return value >> 8, value & 0xFF


def decode_task_duration_us(value: int) -> tuple[int, int]:
    return value >> 32, value & 0xFFFF_FFFF


def counter_rows(records: list[dict[str, Any]], names: dict[int, str]) -> list[tuple[int, str, int]]:
    rows: list[tuple[int, str, int]] = []
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        rows.append((record_ts(record), names.get(cid, f"counter_0x{cid:x}"), value))
    return rows


def analyze_futex_ops(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    begins: dict[str, dict[str, Any]] = {}
    args: dict[str, dict[str, int]] = {}
    wait_tasks: dict[str, int] = {}
    rows: list[dict[str, Any]] = []

    for record in records:
        kind = record.get("kind")
        span = record.get("span")
        parent = record.get("parent")
        payload = record.get("payload") or {}
        if kind == "SpanBegin" and payload.get("sysno") == 98 and isinstance(span, str):
            begins[span] = record
            args[span] = {}
        elif kind == "Instant" and isinstance(parent, str) and parent in args:
            name_id = parse_u32_id(record.get("name_id"))
            if name_id in ARG_NAMES:
                value = payload_u64(payload, "value0")
                if value is not None:
                    args[parent][ARG_NAMES[name_id]] = value
        elif kind == "SpanBegin" and isinstance(parent, str):
            name = event_name(record, names)
            task = payload_u64(payload, "task_id_low")
            if name == "drive.FutexWaitOp" and isinstance(task, int):
                wait_tasks[parent] = task
        elif kind == "SpanEnd" and isinstance(span, str) and span in begins:
            begin = begins[span]
            a = args.get(span, {})
            op_raw = a.get("a1", 0)
            base = op_raw & 0x7F
            flags = op_raw & ~0x7F
            rows.append(
                {
                    "dur": record_ts(record) - record_ts(begin),
                    "span": span,
                    "op": FUTEX_OPS.get(base, f"op{base}"),
                    "op_raw": op_raw,
                    "flags": flags,
                    "uaddr": a.get("a0"),
                    "val": a.get("a2"),
                    "timeout": a.get("a3"),
                    "task": wait_tasks.get(span),
                    "ret": payload.get("ret"),
                    "errno": payload.get("errno"),
                }
            )

    if not rows:
        return "\nfutex ops: none"

    groups: dict[tuple[str, int | None], dict[str, Any]] = {}
    for row in rows:
        key = (row["op"], row["uaddr"])
        group = groups.setdefault(
            key,
            {
                "op": row["op"],
                "uaddr": row["uaddr"],
                "n": 0,
                "sum": 0,
                "max": 0,
                "rets": Counter(),
                "tasks": Counter(),
            },
        )
        group["n"] += 1
        group["sum"] += row["dur"]
        group["max"] = max(group["max"], row["dur"])
        group["rets"][(row.get("ret"), row.get("errno"))] += 1
        if row.get("task") is not None:
            group["tasks"][row["task"]] += 1

    out = ["", "futex ops:"]
    for row in sorted(groups.values(), key=lambda r: r["sum"], reverse=True)[:top]:
        uaddr = "None" if row["uaddr"] is None else hex(row["uaddr"])
        rets = " ".join(f"{ret}/{err}:{n}" for (ret, err), n in row["rets"].most_common(4))
        tasks = " ".join(f"{task}:{n}" for task, n in row["tasks"].most_common(4))
        out.append(
            f"{row['op'][:12].ljust(12)} uaddr={uaddr:<12} n={row['n']:>4} "
            f"total={fmt_ns(row['sum']):>10} avg={fmt_ns(row['sum']/row['n']):>10} "
            f"max={fmt_ns(row['max']):>10} ret/errno={rets} tasks={tasks}"
        )

    out.append("")
    out.append("slowest futex ops:")
    for row in sorted(rows, key=lambda r: r["dur"], reverse=True)[:top]:
        uaddr = "None" if row["uaddr"] is None else hex(row["uaddr"])
        out.append(
            f"{row['span']} {row['op']} uaddr={uaddr} task={row.get('task')} "
            f"val={row.get('val')} flags=0x{row.get('flags', 0):x} "
            f"dur={fmt_ns(row['dur'])} ret={row.get('ret')} errno={row.get('errno')}"
        )
    return "\n".join(out)


def allocation_rows(
    records: list[dict[str, Any]],
    names: dict[int, str],
    derived: DerivedTables | None = None,
) -> list[tuple[int, str, str, int]]:
    derived = derived or build_derived_tables(records)
    rows: list[tuple[int, str, str, int]] = []
    for row in derived.allocation_rows:
        name_id = row.get("name_id")
        if isinstance(name_id, int):
            name = names.get(name_id, f"name_0x{name_id:x}")
        else:
            name = "unknown"
        rows.append((int(row["ts"]), str(row["track"]), name, int(row["value"])))
    return rows


def analyze_allocation_tracks(
    records: list[dict[str, Any]],
    names: dict[int, str],
    top: int,
    derived: DerivedTables | None = None,
) -> str:
    rows = allocation_rows(records, names, derived)
    if not rows:
        return "\nallocation tracks: none"

    grouped: dict[tuple[str, str], list[int]] = defaultdict(list)
    for _ts, track, name, value in rows:
        grouped[(track, name)].append(value)

    out = ["", "allocation tracks:"]
    for (track, name), values in sorted(
        grouped.items(), key=lambda item: (sum(item[1]), len(item[1])), reverse=True
    )[:top]:
        total = sum(values)
        avg = total / len(values)
        if name.endswith(".duration_ns"):
            out.append(
                f"{track} {name} n={len(values):>6} total={fmt_ns(total):>11} "
                f"avg={fmt_ns(avg):>10} p50={fmt_ns(percentile(values, 0.50)):>10} "
                f"p95={fmt_ns(percentile(values, 0.95)):>10} "
                f"p99={fmt_ns(percentile(values, 0.99)):>10} max={fmt_ns(max(values)):>10}"
            )
        else:
            out.append(
                f"{track} {name} n={len(values):>6} sum={total:>10} "
                f"avg={avg:>8.1f} p50={percentile(values, 0.50):>6} "
                f"p95={percentile(values, 0.95):>6} p99={percentile(values, 0.99):>6} "
                f"max={max(values):>6}"
            )

    out.append("recent allocation markers:")
    for ts, track, name, value in rows[-top:]:
        out.append(f"  ts={ts} {track} {name} value={value}")
    return "\n".join(out)


def analyze_lock_metrics(
    records: list[dict[str, Any]],
    names: dict[int, str],
    top: int,
    derived: DerivedTables | None = None,
) -> str:
    derived = derived or build_derived_tables(records)
    if not derived.lock_rows:
        return "\nlock metrics: none"

    metrics_by_lock: dict[int, dict[str, list[int]]] = defaultdict(lambda: defaultdict(list))
    metric_names = {
        fnv1a32("debug.lock.wait_ns"): "wait",
        fnv1a32("debug.lock.service_ns"): "service",
        fnv1a32("debug.lock.response_ns"): "response",
        fnv1a32("debug.lock.spins"): "spins",
        fnv1a32("debug.lock.contended"): "contended",
    }
    first_ts_by_lock: dict[int, int] = {}
    last_ts_by_lock: dict[int, int] = {}
    for row in derived.lock_rows:
        lock_id = int(row["lock_id"])
        metric_id = int(row["metric_id"])
        metric_name = metric_names.get(metric_id, names.get(metric_id, f"metric_0x{metric_id:x}"))
        metrics_by_lock[lock_id][metric_name].append(int(row["value"]))
        ts = int(row["ts"])
        first_ts_by_lock[lock_id] = min(first_ts_by_lock.get(lock_id, ts), ts)
        last_ts_by_lock[lock_id] = max(last_ts_by_lock.get(lock_id, ts), ts)

    def score(item: tuple[int, dict[str, list[int]]]) -> int:
        _lock_id, metrics = item
        values = metrics.get("response") or metrics.get("wait") or metrics.get("service") or []
        return percentile(values, 0.99) if values else 0

    out = ["", "lock metrics:"]
    for lock_id, metrics in sorted(metrics_by_lock.items(), key=score, reverse=True)[:top]:
        name = names.get(lock_id, f"lock_0x{lock_id:x}")
        window = max(1, last_ts_by_lock[lock_id] - first_ts_by_lock[lock_id])
        service = metrics.get("service", [])
        waits = metrics.get("wait", [])
        response = metrics.get("response", [])
        service_sum = sum(service)
        rho = min(0.999999, service_sum / window) if service else 0.0
        predicted = ""
        if service:
            service_avg = service_sum / len(service)
            predicted = f" rho={rho:.3f} Rq={fmt_ns(service_avg / max(1.0e-9, 1.0 - rho))}"
        wait_summary = (
            "wait n=0"
            if not waits
            else (
                f"wait n={len(waits)} p50={fmt_ns(percentile(waits, 0.50))} "
                f"p99={fmt_ns(percentile(waits, 0.99))} max={fmt_ns(max(waits))}"
            )
        )
        service_summary = (
            ""
            if not service
            else (
                f" service p50={fmt_ns(percentile(service, 0.50))} "
                f"p99={fmt_ns(percentile(service, 0.99))} max={fmt_ns(max(service))}"
            )
        )
        response_summary = (
            ""
            if not response
            else (
                f" response p50={fmt_ns(percentile(response, 0.50))} "
                f"p99={fmt_ns(percentile(response, 0.99))} max={fmt_ns(max(response))}"
            )
        )
        spins = sum(metrics.get("spins", []))
        contended = sum(metrics.get("contended", []))
        out.append(
            f"{name[:40].ljust(40)} {wait_summary}{service_summary}{response_summary} "
            f"spins={spins} contended={contended}{predicted}"
        )
    return "\n".join(out)


def analyze_sched_counters(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    events: list[tuple[int, str, int, int | None, int | None]] = []
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if not name.startswith("debug.sched."):
            continue
        task: int | None = None
        code: int | None = None
        if name == "debug.sched.runnable.front":
            task = value >> 1
            code = value & 1
        elif name == "debug.sched.runnable.hint":
            task, code = decode_task_code(value)
        elif name in {
            "debug.sched.submit.queue",
            "debug.sched.pick.queue",
            "debug.sched.runnable.queue",
            "debug.sched.stop.reason",
        }:
            task, code = decode_task_code(value)
        events.append((record_ts(record), name, value, task, code))

    if not events:
        return "\nscheduler counters: none"

    counts = Counter(name for _, name, _, _, _ in events)
    runnable_at: dict[int, tuple[int, int]] = {}
    pick_delays: list[tuple[int, int, int, int]] = []
    for ts, name, _value, task, code in events:
        if task is None:
            continue
        if name == "debug.sched.runnable.queue":
            runnable_at[task] = (ts, code or 0)
        elif name == "debug.sched.pick.queue" and task in runnable_at:
            start, queue_code = runnable_at.pop(task)
            pick_delays.append((ts - start, task, queue_code, code or 0))

    out = ["", "scheduler counters:"]
    out.append("counts: " + " ".join(f"{name}={count}" for name, count in counts.most_common()))
    if pick_delays:
        out.append("slowest runnable->pick:")
        for delay, task, runnable_queue, pick_queue in sorted(pick_delays, reverse=True)[:top]:
            out.append(
                f"task={task:<4} runnable={QUEUE_NAMES.get(runnable_queue, runnable_queue)} "
                f"picked={QUEUE_NAMES.get(pick_queue, pick_queue)} delay={fmt_ns(delay)}"
            )

    stop_counts: Counter[str] = Counter()
    queue_counts: Counter[str] = Counter()
    hint_counts: Counter[str] = Counter()
    for _ts, name, _value, _task, code in events:
        if code is None:
            continue
        if name == "debug.sched.stop.reason":
            stop_counts[STOP_REASON_NAMES.get(code, str(code))] += 1
        elif name == "debug.sched.runnable.hint":
            hint_counts[WAKE_HINT_NAMES.get(code, str(code))] += 1
        elif name in {
            "debug.sched.submit.queue",
            "debug.sched.pick.queue",
            "debug.sched.runnable.queue",
        }:
            queue_counts[f"{name.split('.')[-2]}:{QUEUE_NAMES.get(code, str(code))}"] += 1
    if queue_counts:
        out.append("queue hist: " + " ".join(f"{k}={v}" for k, v in queue_counts.most_common()))
    if hint_counts:
        out.append("hint hist: " + " ".join(f"{k}={v}" for k, v in hint_counts.most_common()))
    if stop_counts:
        out.append("stop hist: " + " ".join(f"{k}={v}" for k, v in stop_counts.most_common()))
    return "\n".join(out)


def analyze_wake_hint_counters(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    rows: list[tuple[int, str, int, int]] = []
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name not in {"debug.wake.pending_hint", "debug.wake.drain_hint"}:
            continue
        task, code = decode_task_code(value)
        rows.append((record_ts(record), name, task, code))

    if not rows:
        return "\nwake hint counters: none"

    by_name_hint: Counter[str] = Counter()
    by_task_hint: Counter[str] = Counter()
    for _ts, name, task, code in rows:
        hint = MAILBOX_HINT_NAMES.get(code, str(code))
        by_name_hint[f"{name.split('.')[-1]}:{hint}"] += 1
        by_task_hint[f"task={task}:{hint}"] += 1

    out = ["", "wake hint counters:"]
    out.append("hist: " + " ".join(f"{k}={v}" for k, v in by_name_hint.most_common()))
    out.append("by task: " + " ".join(f"{k}={v}" for k, v in by_task_hint.most_common(top)))
    out.append("recent wake hints:")
    for ts, name, task, code in rows[-top:]:
        out.append(
            f"ts={ts} {name} task={task} hint={MAILBOX_HINT_NAMES.get(code, str(code))}"
        )
    return "\n".join(out)


VM_POLL_ATTR_PHASE_PAIRS = [
    ("debug.vm.fault.resolve.phase", 0, 1),
    ("debug.vm.fault.publish.phase", 0, 1),
    ("debug.vm.fault.publish.phase", 2, 3),
    ("debug.vm.private_set.install.phase", 2, 3),
    ("debug.vm.fault.publish.phase", 3, 4),
    ("debug.vm.pmap.publish_batch.insert.phase", 1, 2),
]

VM_WAIT_MARKERS = {
    "debug.vm.fault.resolve.wait",
    "debug.vm.fault.publish.wait",
    "debug.vm.fault.script.wait",
    "debug.vm.fault.materialize.wait",
}


def analyze_vm_poll_attribution(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    counters = counter_rows(records, names)
    poll_stack: dict[int, list[int]] = defaultdict(list)
    pending_duration_us: dict[int, int] = {}
    poll_intervals: list[dict[str, int]] = []
    sched_stops: list[tuple[int, int, int]] = []
    sched_picks: list[tuple[int, int, int]] = []
    phase_events: dict[str, list[tuple[int, int]]] = defaultdict(list)
    wait_events: list[tuple[int, str, int]] = []
    size_events: list[tuple[int, str, int]] = []

    target_phase_names = {name for name, _start, _end in VM_POLL_ATTR_PHASE_PAIRS}
    size_names = {
        "debug.vm.fault.publish.pmap_mapped_pages",
        "debug.vm.fault.publish.private_len",
    }

    for ts, name, value in counters:
        if name == "debug.reactor.poll.begin":
            poll_stack[value].append(ts)
        elif name == "debug.reactor.poll.consumed_us":
            task, consumed_us = decode_task_duration_us(value)
            pending_duration_us[task] = consumed_us
        elif name == "debug.reactor.poll.end":
            task = value
            starts = poll_stack.get(task)
            if starts:
                begin = starts.pop()
                poll_intervals.append(
                    {
                        "task": task,
                        "begin": begin,
                        "end": ts,
                        "consumed_us": pending_duration_us.pop(task, -1),
                    }
                )
        elif name == "debug.sched.stop.reason":
            task, reason = decode_task_code(value)
            sched_stops.append((ts, task, reason))
        elif name == "debug.sched.pick.queue":
            task, queue = decode_task_code(value)
            sched_picks.append((ts, task, queue))
        elif name in target_phase_names:
            phase_events[name].append((ts, value))
        elif name in size_names:
            size_events.append((ts, name, value))
        elif name in VM_WAIT_MARKERS:
            wait_events.append((ts, name, value))

    poll_intervals.sort(key=lambda row: row["begin"])

    def containing_poll(ts: int) -> dict[str, int] | None:
        for row in poll_intervals:
            if row["begin"] <= ts <= row["end"]:
                return row
        return None

    def stops_between(start: int, end: int, task: int | None = None) -> list[tuple[int, int, int]]:
        return [
            row
            for row in sched_stops
            if start < row[0] < end and (task is None or row[1] == task)
        ]

    def picks_between(start: int, end: int, task: int | None = None) -> list[tuple[int, int, int]]:
        return [
            row
            for row in sched_picks
            if start < row[0] < end and (task is None or row[1] == task)
        ]

    def classify_gap(start_ts: int, end_ts: int) -> tuple[str, str]:
        start_poll = containing_poll(start_ts)
        end_poll = containing_poll(end_ts)
        if start_poll is None or end_poll is None:
            return "unknown", "missing_poll_marker"
        start_task = start_poll["task"]
        end_task = end_poll["task"]
        if start_poll is end_poll:
            return (
                "same_poll",
                f"task={start_task} poll={fmt_ns(start_poll['end'] - start_poll['begin'])} "
                f"accounted={start_poll['consumed_us']}us",
            )
        if start_task == end_task:
            stops = stops_between(start_ts, end_ts, start_task)
            picks = picks_between(start_ts, end_ts, start_task)
            if stops or picks:
                stop_names = ",".join(
                    STOP_REASON_NAMES.get(reason, str(reason)) for _ts, _task, reason in stops[:3]
                )
                pick_names = ",".join(
                    QUEUE_NAMES.get(queue, str(queue)) for _ts, _task, queue in picks[:3]
                )
                return (
                    "cross_poll_same_task",
                    f"task={start_task} stops={stop_names or 'none'} picks={pick_names or 'none'}",
                )
            return "cross_poll_same_task", f"task={start_task} no_stop_pick_between"
        return "cross_poll_other_task", f"start_task={start_task} end_task={end_task}"

    def size_snapshot(start_ts: int, end_ts: int) -> dict[str, int]:
        snapshot: dict[str, int] = {}
        for ts, name, value in size_events:
            if start_ts <= ts <= end_ts:
                snapshot[name] = value
        return snapshot

    def bucket_size(value: int) -> str:
        if value < 0:
            return "unknown"
        if value == 0:
            return "0"
        if value <= 16:
            return "1-16"
        if value <= 64:
            return "17-64"
        if value <= 256:
            return "65-256"
        if value <= 1024:
            return "257-1024"
        if value <= 4096:
            return "1025-4096"
        return ">4096"

    out = ["", "vm poll attribution:"]
    out.append(
        f"poll_intervals={len(poll_intervals)} unclosed_polls={sum(len(v) for v in poll_stack.values())}"
    )
    if not poll_intervals:
        out.append("no poll markers present")
        return "\n".join(out)

    for phase_name, start_value, end_value in VM_POLL_ATTR_PHASE_PAIRS:
        starts: list[int] = []
        rows: list[dict[str, Any]] = []
        for ts, value in phase_events.get(phase_name, []):
            if value == start_value:
                starts.append(ts)
            elif value == end_value and starts:
                start_ts = starts.pop()
                classification, detail = classify_gap(start_ts, ts)
                rows.append(
                    {
                        "delta": ts - start_ts,
                        "start": start_ts,
                        "end": ts,
                        "classification": classification,
                        "detail": detail,
                        "sizes": size_snapshot(start_ts, ts),
                    }
                )

        if not rows:
            out.append(f"{phase_name} {start_value}->{end_value}: none")
            continue
        values = sorted(int(row["delta"]) for row in rows)
        classes = Counter(str(row["classification"]) for row in rows)
        total = sum(values)
        out.append(
            f"{phase_name} {start_value}->{end_value}: n={len(rows)} "
            f"total={fmt_ns(total)} avg={fmt_ns(total/len(values))} "
            f"p50={fmt_ns(percentile(values, 0.50))} "
            f"p95={fmt_ns(percentile(values, 0.95))} max={fmt_ns(values[-1])} "
            f"class={' '.join(f'{k}={v}' for k, v in classes.most_common())}"
        )
        if phase_name == "debug.vm.fault.publish.phase" and start_value == 2 and end_value == 3:
            for metric_name in [
                "debug.vm.fault.publish.pmap_mapped_pages",
                "debug.vm.fault.publish.private_len",
            ]:
                bucketed: dict[str, list[int]] = defaultdict(list)
                for row in rows:
                    size = dict(row.get("sizes") or {}).get(metric_name, -1)
                    bucketed[bucket_size(int(size))].append(int(row["delta"]))
                if bucketed:
                    out.append(f"  by {metric_name}:")
                    for bucket, bucket_values in sorted(bucketed.items()):
                        vals = sorted(bucket_values)
                        bucket_total = sum(vals)
                        out.append(
                            f"    {bucket}: n={len(vals)} avg={fmt_ns(bucket_total/len(vals))} "
                            f"p50={fmt_ns(percentile(vals, 0.50))} "
                            f"p95={fmt_ns(percentile(vals, 0.95))} max={fmt_ns(vals[-1])}"
                        )
        for row in sorted(rows, key=lambda item: int(item["delta"]), reverse=True)[:top]:
            sizes = row.get("sizes") or {}
            size_detail = ""
            if sizes:
                rendered = " ".join(
                    f"{name.rsplit('.', 1)[-1]}={value}" for name, value in sorted(sizes.items())
                )
                size_detail = f" {rendered}"
            out.append(
                f"  dt={fmt_ns(int(row['delta']))} class={row['classification']} {row['detail']}{size_detail}"
            )

    if wait_events:
        out.append("vm range-lock/materialize wait markers:")
        by_name = Counter(name for _ts, name, _value in wait_events)
        out.append("counts: " + " ".join(f"{name}={count}" for name, count in by_name.most_common()))
        for ts, name, value in wait_events[-top:]:
            poll = containing_poll(ts)
            if poll is None:
                detail = "poll=unknown"
            else:
                task = poll["task"]
                next_stop = next((row for row in sched_stops if row[0] >= ts and row[1] == task), None)
                next_pick = next((row for row in sched_picks if row[0] >= ts and row[1] == task), None)
                stop_detail = (
                    "stop=n/a"
                    if next_stop is None
                    else f"stop={STOP_REASON_NAMES.get(next_stop[2], next_stop[2])}@{fmt_ns(next_stop[0] - ts)}"
                )
                pick_detail = (
                    "pick=n/a"
                    if next_pick is None
                    else f"pick={QUEUE_NAMES.get(next_pick[2], next_pick[2])}@{fmt_ns(next_pick[0] - ts)}"
                )
                detail = f"task={task} {stop_detail} {pick_detail}"
            out.append(f"  ts={ts} {name} value={value} {detail}")
    else:
        out.append("vm range-lock/materialize wait markers: none")

    return "\n".join(out)


def analyze_futex_table_counters(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    rows: list[tuple[int, str, int]] = []
    interesting = (
        "debug.futex.wait_table.",
        "debug.futex.wake_table.",
        "debug.futex.cancel_table.",
        "debug.futex.requeue_table.",
        "debug.futex.wake_decision.",
    )
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name.startswith(interesting):
            rows.append((record_ts(record), name, value))

    if not rows:
        return "\nfutex table snapshots: none"

    latest: dict[str, int] = {}
    counts: Counter[str] = Counter()
    for _ts, name, value in rows:
        latest[name] = value
        counts[name] += 1

    out = ["", "futex table snapshots:"]
    out.append("counts: " + " ".join(f"{name}={count}" for name, count in counts.most_common(top)))
    out.append("latest:")
    for name, value in sorted(latest.items())[:top]:
        rendered = fmt_counter_value(name, value)
        out.append(f"  {name}: {rendered}")
    out.append("recent:")
    for ts, name, value in rows[-top:]:
        rendered = fmt_counter_value(name, value)
        out.append(f"  ts={ts} {name}={rendered}")
    return "\n".join(out)


def analyze_wait_source_notify(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    rows: list[tuple[int, int, int, int, int, str]] = []
    for record in records:
        if record.get("kind") != "Instant":
            continue
        payload = record.get("payload") or {}
        source = payload_u64(payload, "source_id_low")
        mask = payload_u64(payload, "mask_bits")
        task = payload_u64(payload, "task_id_low")
        generation = payload_u64(payload, "wait_generation_low")
        if not all(v is not None for v in [source, mask, task, generation]):
            continue
        name_id = parse_u32_id(record.get("name_id"))
        name = names.get(name_id, f"name_0x{name_id:x}") if name_id is not None else "unknown"
        rows.append((record_ts(record), source, mask, task, generation, name))

    if not rows:
        return "\nwait-source notify list: none"

    by_source: Counter[str] = Counter()
    by_task: Counter[str] = Counter()
    for _ts, source, mask, task, generation, _name in rows:
        by_source[f"source=0x{source:x}:mask=0x{mask:x}"] += 1
        by_task[f"task={task}:gen={generation}"] += 1

    out = ["", "wait-source notify list:"]
    out.append("by source: " + " ".join(f"{key}={count}" for key, count in by_source.most_common(top)))
    out.append("by task: " + " ".join(f"{key}={count}" for key, count in by_task.most_common(top)))
    out.append("recent:")
    for ts, source, mask, task, generation, name in rows[-top:]:
        out.append(
            f"  ts={ts} {name} source=0x{source:x} mask=0x{mask:x} task={task} gen={generation}"
        )
    return "\n".join(out)


def analyze_futex_source_correlation(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    snapshots: list[dict[str, int | str]] = []
    notify: list[tuple[int, int, int, int, int]] = []
    pending: dict[str, dict[str, int | str]] = defaultdict(dict)
    table_prefixes = (
        "debug.futex.wait_table.",
        "debug.futex.wake_table.",
        "debug.futex.cancel_table.",
        "debug.futex.requeue_table.",
    )

    for record in records:
        payload = record.get("payload") or {}
        if record.get("kind") == "Counter":
            cid = payload.get("counter_id")
            value = payload_u64(payload, "value")
            if not isinstance(cid, int) or value is None:
                continue
            name = names.get(cid, f"counter_0x{cid:x}")
            prefix = next((p for p in table_prefixes if name.startswith(p)), None)
            if prefix is None:
                continue
            phase = prefix.removeprefix("debug.futex.").removesuffix(".")
            slot = pending[prefix]
            slot["phase"] = phase
            slot["ts"] = record_ts(record)
            if name.endswith(".sample.uaddr"):
                slot["uaddr"] = value
            elif name.endswith(".sample.waiters"):
                slot["waiters"] = value
            elif name.endswith(".sample.mask"):
                slot["mask"] = value
            elif name.endswith(".sample.source"):
                slot["source"] = value
            elif name.endswith(".sample.subscribers"):
                slot["subscribers"] = value
                if "source" in slot:
                    snapshots.append(dict(slot))
                    pending[prefix] = {}
        elif record.get("kind") == "Instant":
            source = payload_u64(payload, "source_id_low")
            mask = payload_u64(payload, "mask_bits")
            task = payload_u64(payload, "task_id_low")
            generation = payload_u64(payload, "wait_generation_low")
            if all(v is not None for v in [source, mask, task, generation]):
                notify.append((record_ts(record), source, mask, task, generation))

    if not snapshots and not notify:
        return "\nfutex source correlation: none"

    registered_sources = {int(row["source"]) & 0xffff_ffff for row in snapshots if "source" in row}
    notified_sources = {source for _ts, source, _mask, _task, _generation in notify}
    registered_notified = sorted(registered_sources & notified_sources)
    registered_unnotified = sorted(registered_sources - notified_sources)
    notified_unregistered = sorted(notified_sources - registered_sources)

    out = ["", "futex source correlation:"]
    out.append(
        "sets: "
        f"registered={len(registered_sources)} notified={len(notified_sources)} "
        f"matched={len(registered_notified)} registered_unnotified={len(registered_unnotified)} "
        f"notified_unregistered={len(notified_unregistered)}"
    )
    if registered_notified:
        out.append(
            "matched sources: "
            + " ".join(f"0x{source:x}" for source in registered_notified[:top])
        )
    if registered_unnotified:
        out.append(
            "registered without notify: "
            + " ".join(f"0x{source:x}" for source in registered_unnotified[:top])
        )
    if notified_unregistered:
        out.append(
            "notify without sampled registration: "
            + " ".join(f"0x{source:x}" for source in notified_unregistered[:top])
        )

    out.append("recent registrations:")
    for row in snapshots[-top:]:
        source = int(row.get("source", 0))
        uaddr = int(row.get("uaddr", 0))
        waiters = int(row.get("waiters", 0))
        mask = int(row.get("mask", 0))
        subscribers = int(row.get("subscribers", 0))
        phase = str(row.get("phase", "unknown"))
        ts = int(row.get("ts", 0))
        out.append(
            f"  ts={ts} {phase} source=0x{source & 0xffff_ffff:x} raw=0x{source:x} "
            f"uaddr=0x{uaddr:x} waiters={waiters} mask=0x{mask:x} subscribers={subscribers}"
        )
    out.append("recent notified tasks:")
    for ts, source, mask, task, generation in notify[-top:]:
        out.append(
            f"  ts={ts} source=0x{source:x} mask=0x{mask:x} task={task} gen={generation}"
        )
    return "\n".join(out)


def analyze_futex_wake_latency(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    notifications: list[dict[str, int]] = []
    wake_drains: dict[int, list[int]] = defaultdict(list)
    runnable: dict[int, list[int]] = defaultdict(list)
    picks: dict[int, list[int]] = defaultdict(list)
    pick_timeline: list[tuple[int, int]] = []
    stop_timeline: list[tuple[int, int, int]] = []

    for record in records:
        ts = record_ts(record)
        payload = record.get("payload") or {}
        if record.get("kind") == "Instant":
            source = payload_u64(payload, "source_id_low")
            task = payload_u64(payload, "task_id_low")
            generation = payload_u64(payload, "wait_generation_low")
            mask = payload_u64(payload, "mask_bits")
            if all(v is not None for v in [source, task, generation, mask]):
                notifications.append(
                    {
                        "ts": ts,
                        "source": source,
                        "task": task,
                        "generation": generation,
                        "mask": mask,
                    }
                )
            continue
        if record.get("kind") != "Counter":
            continue
        cid = payload.get("counter_id")
        value = payload_u64(payload, "value")
        if not isinstance(cid, int) or value is None:
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        task, code = decode_task_code(value)
        if name == "debug.wake.drain_hint" and code in {1, 2, 3, 4}:
            wake_drains[task].append(ts)
        elif name == "debug.sched.runnable.queue":
            runnable[task].append(ts)
        elif name == "debug.sched.pick.queue":
            picks[task].append(ts)
            pick_timeline.append((ts, task))
        elif name == "debug.sched.stop.reason":
            stop_timeline.append((ts, task, code))

    if not notifications:
        return "\nfutex wake latency: none"

    def next_at_or_after(events: dict[int, list[int]], task: int, after: int) -> int | None:
        rows = events.get(task, [])
        for candidate in rows:
            if candidate >= after:
                return candidate
        return None

    rows: list[dict[str, int | None]] = []
    for note in notifications:
        task = note["task"]
        notify_ts = note["ts"]
        current_task = None
        for pick_ts, picked_task in pick_timeline:
            if pick_ts > notify_ts:
                break
            current_task = picked_task
        current_stop_ts = None
        current_stop_reason = None
        if current_task is not None:
            for stop_ts, stopped_task, stop_reason in stop_timeline:
                if stop_ts >= notify_ts and stopped_task == current_task:
                    current_stop_ts = stop_ts
                    current_stop_reason = stop_reason
                    break
        pick_after_notify = next_at_or_after(picks, task, notify_ts)
        drain_after_notify = next_at_or_after(wake_drains, task, notify_ts)
        if (
            pick_after_notify is not None
            and drain_after_notify is not None
            and pick_after_notify < drain_after_notify
        ):
            drain_ts = None
            runnable_ts = None
            path = "picked_before_drain"
        else:
            drain_ts = drain_after_notify
            runnable_ts = next_at_or_after(runnable, task, drain_ts or notify_ts)
            path = "drained" if drain_ts is not None else "undrained"
            if (
                runnable_ts is not None
                and pick_after_notify is not None
                and pick_after_notify < runnable_ts
            ):
                runnable_ts = None
                path = "picked_before_runnable"
        pick_ts = pick_after_notify
        rows.append(
            {
                **note,
                "drain_ts": drain_ts,
                "runnable_ts": runnable_ts,
                "pick_ts": pick_ts,
                "path": path,
                "intermediate_picks": [
                    picked_task
                    for ts, picked_task in pick_timeline
                    if runnable_ts is not None
                    and pick_ts is not None
                    and runnable_ts < ts < pick_ts
                ],
                "current_task": current_task,
                "current_stop_ts": current_stop_ts,
                "current_stop_reason": current_stop_reason,
            }
        )

    out = ["", "futex wake latency:"]
    complete = [row for row in rows if row["drain_ts"] is not None and row["pick_ts"] is not None]
    path_counts = Counter(str(row.get("path", "unknown")) for row in rows)
    if complete:
        notify_to_drain = [int(row["drain_ts"]) - int(row["ts"]) for row in complete]
        drain_to_pick = [int(row["pick_ts"]) - int(row["drain_ts"]) for row in complete]
        notify_to_pick = [int(row["pick_ts"]) - int(row["ts"]) for row in complete]
        intermediate_picks = Counter(
            task
            for row in complete
            for task in (row.get("intermediate_picks") or [])
        )
        out.append(
            "summary: "
            f"complete={len(complete)} "
            f"notify->drain avg={fmt_ns(sum(notify_to_drain)/len(notify_to_drain))} "
            f"max={fmt_ns(max(notify_to_drain))}; "
            f"drain->pick avg={fmt_ns(sum(drain_to_pick)/len(drain_to_pick))} "
            f"max={fmt_ns(max(drain_to_pick))}; "
            f"notify->pick avg={fmt_ns(sum(notify_to_pick)/len(notify_to_pick))} "
            f"max={fmt_ns(max(notify_to_pick))}"
        )
        if intermediate_picks:
            out.append(
                "intermediate picks: "
                + " ".join(f"task={task}:{count}" for task, count in intermediate_picks.most_common(top))
            )
    out.append("paths: " + " ".join(f"{path}={count}" for path, count in path_counts.most_common()))
    missing = len(rows) - len(complete)
    if missing:
        out.append(f"incomplete={missing}")

    for row in sorted(
        rows,
        key=lambda r: ((r["pick_ts"] or r["drain_ts"] or r["ts"]) - r["ts"]),
        reverse=True,
    )[:top]:
        drain_ts = row["drain_ts"]
        runnable_ts = row["runnable_ts"]
        pick_ts = row["pick_ts"]
        notify_to_drain = "n/a" if drain_ts is None else fmt_ns(drain_ts - int(row["ts"]))
        drain_to_runnable = (
            "n/a"
            if drain_ts is None or runnable_ts is None
            else fmt_ns(runnable_ts - drain_ts)
        )
        runnable_to_pick = (
            "n/a"
            if runnable_ts is None or pick_ts is None
            else fmt_ns(pick_ts - runnable_ts)
        )
        notify_to_pick = "n/a" if pick_ts is None else fmt_ns(pick_ts - int(row["ts"]))
        current_task = row["current_task"]
        current_stop_ts = row["current_stop_ts"]
        current_stop_reason = row["current_stop_reason"]
        if current_task is None:
            blocker = "running=unknown"
        elif current_stop_ts is None:
            blocker = f"running={current_task} stop=n/a"
        else:
            blocker = (
                f"running={current_task} "
                f"stop={STOP_REASON_NAMES.get(int(current_stop_reason or 0), current_stop_reason)} "
                f"after={fmt_ns(int(current_stop_ts) - int(row['ts']))}"
            )
        intermediate = Counter(row.get("intermediate_picks") or [])
        between = ""
        if intermediate:
            between = " between=" + ",".join(
                f"{task}:{count}" for task, count in intermediate.most_common(6)
            )
        out.append(
            f"task={row['task']} source=0x{int(row['source']):x} "
            f"notify->drain={notify_to_drain} drain->runnable={drain_to_runnable} "
            f"runnable->pick={runnable_to_pick} notify->pick={notify_to_pick} "
            f"path={row.get('path')} {blocker}{between}"
        )
    return "\n".join(out)


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
            "sched_intervals.parquet, lock_rows.parquet)."
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

    input_path = input_arg_path(args)
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

    records = load_input_records(args)
    derived_result = load_or_build_derived_tables(
        input_path,
        records,
        cache_dir=args.cache_dir,
        dangling_span_policy=args.dangling_spans,
    )
    if derived_result.path is not None:
        state = "hit" if derived_result.hit else "miss"
        print(f"derived cache: {state} {derived_result.path}")
    if args.sql is not None or args.sql_file is not None:
        query = args.sql if args.sql is not None else args.sql_file.read_text()
        print(run_sql_query(records, derived_result.tables, names, query, args.sql_format), end="")
        return 0
    if args.python_file is not None:
        proc = run_python_file(
            args.python_file,
            derived_result.tables,
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
        written = export_derived_tables_parquet(
            derived_result.tables,
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
