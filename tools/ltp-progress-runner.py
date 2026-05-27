#!/usr/bin/env python3
"""Run Txv2 LTP syscall batches in small groups and update progress docs."""

from __future__ import annotations

import argparse
import datetime as dt
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
JUDGE_RE = re.compile(r"^\s*([✓✗~?])\s+(\S+)\s+(\d+)/(\d+)\s*$")
ROW_RE = re.compile(r"^\| `([^`]+)` \| ([^|]+) \| ([^|]+) \| ([^|]*) \|$")
CASE_START_RE = re.compile(r"^RUN LTP CASE ([^ :]+)")

ORDER = [
    "vm",
    "process",
    "cred",
    "signal",
    "ipc",
    "sched",
    "event",
    "time",
    "mount",
    "heavy",
    "aio",
]

STATUS_BY_MARK = {
    "✓": "pass",
    "✗": "fail",
    "~": "partial",
    "?": "skip",
}


def run(cmd: list[str], cwd: Path, timeout: int | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=cwd,
        timeout=timeout,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def strip_ansi(text: str) -> str:
    return ANSI_RE.sub("", text).replace("\r", "")


def batch_cases(repo: Path, batch: str) -> list[str]:
    cp = run(["make", "ltp-batch-cases", f"LTP_BATCH={batch}"], repo)
    if cp.returncode != 0:
        raise RuntimeError(cp.stdout)
    cases: list[str] = []
    for raw in cp.stdout.splitlines():
        line = raw.strip()
        if not line or line.startswith(("make:", "python3 ")):
            continue
        if re.match(r"^[A-Za-z0-9_.-]+$", line):
            cases.append(line)
    return cases


def load_existing(doc: Path) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    if not doc.exists():
        return rows
    for line in doc.read_text(encoding="utf-8").splitlines():
        m = ROW_RE.match(line)
        if not m:
            continue
        case, score, status, note = m.groups()
        if case == "Case":
            continue
        rows[case] = {
            "score": score.strip(),
            "status": status.strip(),
            "note": note.strip(),
        }
    return rows


def parse_blocks(serial: str) -> dict[str, str]:
    blocks: dict[str, list[str]] = {}
    current: str | None = None
    for raw in strip_ansi(serial).splitlines():
        m = CASE_START_RE.match(raw)
        if m:
            current = m.group(1)
            blocks.setdefault(current, []).append(raw)
            continue
        if current is not None:
            blocks.setdefault(current, []).append(raw)
    notes: dict[str, str] = {}
    for case, lines in blocks.items():
        joined = "\n".join(lines)
        note = ""
        for needle in ("panicked at", "trap-action-terminate", "Test timed out"):
            if needle in joined:
                note = needle
                break
        if not note:
            for kind in ("TBROK", "TFAIL", "TCONF", "TWARN"):
                for line in lines:
                    if kind in line:
                        msg = line.split(kind, 1)[-1].strip(" :\t")
                        note = f"{kind}: {msg}"
                        break
                if note:
                    break
        if not note:
            if "ENOSYS" in joined:
                note = "ENOSYS observed"
            elif "EINVAL" in joined:
                note = "EINVAL observed"
            elif "ETIMEDOUT" in joined:
                note = "ETIMEDOUT observed"
        notes[case] = note[:180] if note else ""
    return notes


def parse_judge(output: str) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    for line in strip_ansi(output).splitlines():
        m = JUDGE_RE.match(line)
        if not m:
            continue
        mark, case, got, total = m.groups()
        rows[case] = {
            "score": f"{got}/{total}",
            "status": STATUS_BY_MARK.get(mark, "fail"),
            "note": "",
        }
    return rows


def status_for(row: dict[str, str]) -> str:
    if row["status"] != "fail":
        return row["status"]
    if row.get("note", "").startswith("TCONF"):
        return "skip"
    return "fail"


def write_doc(
    repo: Path,
    batch: str,
    cases: list[str],
    rows: dict[str, dict[str, str]],
    latest: str,
    reached: str,
    log_dir: Path,
    completed: bool,
) -> None:
    today = dt.date.today().isoformat()
    got = total = 0
    for row in rows.values():
        m = re.match(r"^(\d+)/(\d+)$", row["score"])
        if m:
            got += int(m.group(1))
            total += int(m.group(2))
    doc = repo / "docs" / "LTP" / f"ltp-{batch}-progress.md"
    note_counts: dict[str, int] = {}
    for row in rows.values():
        note = row.get("note", "")
        if not note:
            continue
        key = note.split(":", 1)[0]
        note_counts[key] = note_counts.get(key, 0) + 1

    lines = [
        f"# LTP {batch} Progress",
        "",
        f"`{batch}` batch local tracking. Cases are from `tools/ltp-batches.py --batch {batch}`.",
        "Runs are split into explicit 5-case groups with `make oscomp-local-rv64-ltp-musl OSCOMP_LTP=...`.",
        "",
        "## Summary",
        "",
        "| Item | Value | Note |",
        "| --- | ---: | --- |",
        f"| cases | {len(cases)} | from `make ltp-batch-cases LTP_BATCH={batch}` |",
        f"| latest local run | `{latest}` | {today} latest 5-case group |",
        f"| cumulative scored | `{got}/{total}` | recorded rows in this document |",
        f"| reached case | `{reached or 'none'}` | {'batch completed' if completed else 'batch in progress'} |",
        f"| logs | `{log_dir.relative_to(repo)}` | per-group stdout and serial snapshots |",
        "",
        f"## {today} failure notes",
        "",
    ]
    if note_counts:
        for key, count in sorted(note_counts.items(), key=lambda item: (-item[1], item[0])):
            lines.append(f"- {key}: {count} recorded case(s); see per-case notes below.")
    else:
        lines.append("- No failures recorded yet.")
    lines += [
        "",
        "## Cases",
        "",
        "| Case | Score | Status | Note |",
        "| --- | ---: | --- | --- |",
    ]
    for case in cases:
        row = rows.get(case)
        if row is None:
            lines.append(f"| `{case}` | 0/0 | norun |  |")
        else:
            note = row.get("note", "").replace("|", "/")
            lines.append(f"| `{case}` | {row['score']} | {status_for(row)} | {note} |")
    doc.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--batches", default=",".join(ORDER))
    parser.add_argument("--group-size", type=int, default=5)
    parser.add_argument("--timeout", type=int, default=75)
    parser.add_argument("--max-groups", type=int, default=0, help="0 means unlimited")
    parser.add_argument("--start-after", default="", help="force-run cases after this case name in each selected batch")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    log_root = repo / "target" / "oscomp" / "ltp-progress"
    log_root.mkdir(parents=True, exist_ok=True)
    groups_done = 0

    for batch in [b for b in args.batches.split(",") if b]:
        cases = batch_cases(repo, batch)
        doc = repo / "docs" / "LTP" / f"ltp-{batch}-progress.md"
        rows = load_existing(doc)
        if args.start_after and args.start_after in cases:
            pending = cases[cases.index(args.start_after) + 1 :]
        else:
            pending = [case for case in cases if case not in rows or rows[case]["status"] == "norun"]
        print(f"== batch {batch}: {len(rows)} recorded, {len(pending)} pending ==", flush=True)
        batch_log_dir = log_root / batch
        batch_log_dir.mkdir(parents=True, exist_ok=True)
        if not pending:
            write_doc(repo, batch, cases, rows, "no new run", cases[-1] if cases else "", batch_log_dir, True)
            continue
        for i in range(0, len(pending), args.group_size):
            if args.max_groups and groups_done >= args.max_groups:
                return 0
            group = pending[i : i + args.group_size]
            group_name = "+".join(group)
            print(f"-- {batch} group {groups_done + 1}: {group_name}", flush=True)
            cmd = [
                "timeout",
                str(args.timeout),
                "make",
                "oscomp-local-rv64-ltp-musl",
                f"OSCOMP_LTP={','.join(group)}",
            ]
            cp = run(cmd, repo)
            groups_done += 1
            timestamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
            prefix = batch_log_dir / f"{timestamp}-{'_'.join(group)}"
            (prefix.with_suffix(".stdout.log")).write_text(cp.stdout, encoding="utf-8", errors="replace")
            serial_path = repo / "target" / "oscomp" / "os_serial_out_rv.txt"
            serial = serial_path.read_text(encoding="utf-8", errors="replace") if serial_path.exists() else ""
            if serial:
                (prefix.with_suffix(".serial.log")).write_text(serial, encoding="utf-8", errors="replace")
            judge = run(
                ["python3", "tools/oscomp-judge.py", "target/oscomp/os_serial_out_rv.txt", "target/oscomp/testdata"],
                repo,
            )
            (prefix.with_suffix(".judge.log")).write_text(judge.stdout, encoding="utf-8", errors="replace")
            notes = parse_blocks(serial or cp.stdout)
            result_rows = parse_judge(judge.stdout)
            for case in group:
                row = result_rows.get(case)
                if row is None:
                    row = {
                        "score": "0/0",
                        "status": "hang" if cp.returncode == 124 else "fail",
                        "note": "host timeout before judge result" if cp.returncode == 124 else "missing from judge output",
                    }
                note = notes.get(case, "")
                if note:
                    row["note"] = note
                elif cp.returncode == 124 and row["status"] == "hang":
                    row["note"] = "host timeout before case completed"
                rows[case] = row
            latest = judge.stdout.strip().splitlines()[0] if judge.stdout.strip() else f"make exit {cp.returncode}"
            write_doc(repo, batch, cases, rows, latest, group[-1], batch_log_dir, len(rows) >= len(cases))
            print(f"   {latest}", flush=True)
            if cp.returncode == 124:
                print("   host timeout hit; continuing with next group", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
