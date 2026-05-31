#!/usr/bin/env python3
"""Summarize tx-observe replay NDJSON timing.

The input is the JSON stream produced by:

    cargo xtask observe replay --file trace.txtrace --out json

It pairs SpanBegin/SpanEnd records, aggregates syscall/drive duration, and
prints the largest inter-record gaps. The report is deliberately text-first so
it can be pasted into progress notes or debugging handoffs.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

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
]


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


def load_records(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        record = json.loads(line)
        if "ts" in record and "kind" in record:
            records.append(record)
    records.sort(key=lambda r: int(r["ts"]))
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


def describe(record: dict[str, Any], spans: dict[str, dict[str, Any]], names: dict[int, str]) -> str:
    kind = record.get("kind")
    span = record.get("span")
    if kind == "SpanEnd" and span in spans:
        info = spans[span]
        return f"{kind} {span} {info['name']}"
    if kind == "SpanBegin":
        return f"{kind} {span} {event_name(record, names)}"
    if kind == "Counter":
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        cname = names.get(cid, str(cid)) if isinstance(cid, int) else str(cid)
        return f"Counter {cname}={payload.get('value')}"
    return f"{kind} name={record.get('name_id')} span={span} parent={record.get('parent')}"


def analyze(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    begins: dict[str, dict[str, Any]] = {}
    spans: list[dict[str, Any]] = []
    span_by_id: dict[str, dict[str, Any]] = {}
    counters: list[dict[str, Any]] = []
    kinds = Counter(record.get("kind") for record in records)

    for record in records:
        kind = record.get("kind")
        span_id = record.get("span")
        if kind == "SpanBegin" and isinstance(span_id, str):
            begins[span_id] = record
        elif kind == "SpanEnd" and isinstance(span_id, str):
            begin = begins.pop(span_id, None)
            if begin is None:
                continue
            payload = begin.get("payload") or {}
            sysno = payload.get("sysno")
            name = event_name(begin, names)
            span = {
                "span": span_id,
                "name": name,
                "sysno": sysno,
                "begin": int(begin["ts"]),
                "end": int(record["ts"]),
                "dur": int(record["ts"]) - int(begin["ts"]),
                "ret": (record.get("payload") or {}).get("ret"),
                "errno": (record.get("payload") or {}).get("errno"),
            }
            spans.append(span)
            span_by_id[span_id] = span
        elif kind == "Counter":
            counters.append(record)

    out: list[str] = []
    if records:
        out.append(
            f"records={len(records)} window={fmt_ns(records[-1]['ts'] - records[0]['ts'])} "
            f"kinds={dict(kinds)} unclosed_spans={len(begins)}"
        )
    else:
        out.append("records=0")

    grouped: dict[str, dict[str, Any]] = {}
    for span in spans:
        key = f"{span.get('sysno')}:{span['name']}"
        row = grouped.setdefault(
            key,
            {
                "name": span["name"],
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
        out.append(
            f"{span['span']} {span.get('sysno')}:{span['name']} "
            f"dur={fmt_ns(span['dur'])} ret={span.get('ret')} errno={span.get('errno')}"
        )

    gaps = [
        (int(records[i]["ts"]) - int(records[i - 1]["ts"]), records[i - 1], records[i])
        for i in range(1, len(records))
    ]
    out.append("")
    out.append("largest inter-record gaps:")
    for delta, prev, cur in sorted(gaps, key=lambda g: g[0], reverse=True)[:top]:
        out.append(
            f"dt={fmt_ns(delta)} {describe(prev, span_by_id, names)} -> "
            f"{describe(cur, span_by_id, names)}"
        )

    hist: dict[int, Counter[int]] = defaultdict(Counter)
    for record in counters:
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        val = payload.get("value")
        if isinstance(cid, int) and isinstance(val, int):
            hist[cid][val] += 1
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
        ts = int(record["ts"])
        kind = record.get("kind")
        span_id = record.get("span")
        payload = record.get("payload") or {}
        if kind == "Counter":
            cid = payload.get("counter_id")
            val = payload.get("value")
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
        tid = payload.get("value")
        if not isinstance(cid, int) or not isinstance(tid, int):
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name not in CLONE_THREAD_PHASES:
            continue
        per_tid[tid][name] = int(record["ts"])

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
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
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
        per_key[key][name] = int(record["ts"])

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
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
            continue
        rows.append((int(record["ts"]), names.get(cid, f"counter_0x{cid:x}"), value))
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
                value = payload.get("value0")
                if isinstance(value, int):
                    args[parent][ARG_NAMES[name_id]] = value
        elif kind == "SpanBegin" and isinstance(parent, str):
            name = event_name(record, names)
            task = payload.get("task_id_low")
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
                    "dur": int(record["ts"]) - int(begin["ts"]),
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


def analyze_sched_counters(records: list[dict[str, Any]], names: dict[int, str], top: int) -> str:
    events: list[tuple[int, str, int, int | None, int | None]] = []
    for record in records:
        if record.get("kind") != "Counter":
            continue
        payload = record.get("payload") or {}
        cid = payload.get("counter_id")
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
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
        events.append((int(record["ts"]), name, value, task, code))

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
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name not in {"debug.wake.pending_hint", "debug.wake.drain_hint"}:
            continue
        task, code = decode_task_code(value)
        rows.append((int(record["ts"]), name, task, code))

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
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
            continue
        name = names.get(cid, f"counter_0x{cid:x}")
        if name.startswith(interesting):
            rows.append((int(record["ts"]), name, value))

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
        source = payload.get("source_id_low")
        mask = payload.get("mask_bits")
        task = payload.get("task_id_low")
        generation = payload.get("wait_generation_low")
        if not all(isinstance(v, int) for v in [source, mask, task, generation]):
            continue
        name_id = parse_u32_id(record.get("name_id"))
        name = names.get(name_id, f"name_0x{name_id:x}") if name_id is not None else "unknown"
        rows.append((int(record["ts"]), source, mask, task, generation, name))

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
            value = payload.get("value")
            if not isinstance(cid, int) or not isinstance(value, int):
                continue
            name = names.get(cid, f"counter_0x{cid:x}")
            prefix = next((p for p in table_prefixes if name.startswith(p)), None)
            if prefix is None:
                continue
            phase = prefix.removeprefix("debug.futex.").removesuffix(".")
            slot = pending[prefix]
            slot["phase"] = phase
            slot["ts"] = int(record["ts"])
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
            source = payload.get("source_id_low")
            mask = payload.get("mask_bits")
            task = payload.get("task_id_low")
            generation = payload.get("wait_generation_low")
            if all(isinstance(v, int) for v in [source, mask, task, generation]):
                notify.append((int(record["ts"]), source, mask, task, generation))

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
        ts = int(record["ts"])
        payload = record.get("payload") or {}
        if record.get("kind") == "Instant":
            source = payload.get("source_id_low")
            task = payload.get("task_id_low")
            generation = payload.get("wait_generation_low")
            mask = payload.get("mask_bits")
            if all(isinstance(v, int) for v in [source, task, generation, mask]):
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
        value = payload.get("value")
        if not isinstance(cid, int) or not isinstance(value, int):
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ndjson", required=True, type=Path)
    parser.add_argument("--names", type=Path)
    parser.add_argument("--top", type=int, default=20)
    parser.add_argument("--roundtrip-sysno", type=int, default=173)
    parser.add_argument(
        "--source-root",
        type=Path,
        default=Path.cwd(),
        help=(
            "Repository root used to decode in-tree debug.* observe counter "
            "names. Pass /dev/null to disable source scanning."
        ),
    )
    args = parser.parse_args()

    records = load_records(args.ndjson)
    source_root = None if str(args.source_root) == "/dev/null" else args.source_root
    names = load_names(args.names, source_root)
    print(analyze(records, names, args.top))
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
