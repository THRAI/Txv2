"""L5 canonical stream types for Python observe tooling."""

from __future__ import annotations

import struct
from typing import Any

from ..l4_readers import TraceIntegrity

RECORD_MAGIC = 0x5254
SUPPORTED_RECORD_VERSION = 0
SUPPORTED_RECORD_SIZE = 80
MAX_HARTS_DAEMON = 256
RECORD_STRUCT = struct.Struct("<HBBBBBBHHIQQQQIHH16s8s")

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


def repair_record(category: str, hart: int, seq_around: int, details: str) -> dict[str, Any]:
    return {
        "kind": "Repair",
        "category": category,
        "hart": hart,
        "seq_around": seq_around,
        "details": details,
    }


def is_repair_record(record: dict[str, Any]) -> bool:
    return record.get("kind") == "Repair" or str(record.get("category", "")).startswith(
        "txtrace.repair."
    )


def _decode_comm(raw: bytes) -> list[int]:
    return list(raw[:12])


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


class TraceStreamMarker:
    def __init__(
        self,
        kind: str,
        *,
        category: str | None = None,
        hart: int | None = None,
        seq_around: int | None = None,
        details: str | None = None,
        lost_records: int = 0,
        overwritten_records: int = 0,
    ) -> None:
        self.kind = kind
        self.category = category
        self.hart = hart
        self.seq_around = seq_around
        self.details = details
        self.lost_records = lost_records
        self.overwritten_records = overwritten_records

    @classmethod
    def repair(cls, record: dict[str, Any]) -> "TraceStreamMarker":
        return cls(
            "repair",
            category=str(record.get("category")),
            hart=record_hart(record),
            seq_around=parse_u64_id(record.get("seq_around")) or 0,
            details=str(record.get("details", "")),
        )

    @classmethod
    def capture_loss(cls, integrity: TraceIntegrity) -> "TraceStreamMarker":
        return cls(
            "capture_loss",
            lost_records=integrity.lost_records,
            overwritten_records=integrity.overwritten_records,
        )


class TraceEventStream:
    def __init__(
        self,
        records: list[dict[str, Any]],
        integrity: TraceIntegrity,
        markers: list[TraceStreamMarker] | None = None,
    ) -> None:
        self.records = records
        self.integrity = integrity
        self.markers = markers if markers is not None else trace_stream_markers(records, integrity)


def trace_stream_markers(
    records: list[dict[str, Any]],
    integrity: TraceIntegrity,
) -> list[TraceStreamMarker]:
    markers = [TraceStreamMarker.repair(record) for record in records if is_repair_record(record)]
    if integrity.lost_records or integrity.overwritten_records:
        markers.append(TraceStreamMarker.capture_loss(integrity))
    return markers
