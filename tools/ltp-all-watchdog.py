#!/usr/bin/env python3
"""Run the local LTP syscalls sweep with a per-case watchdog.

This is a local bring-up helper, not an official OSComp runner. It feeds the
current `tools/ltp-batches.py --batch all` case list to QEMU in chunks, watches
serial output for the active `RUN LTP CASE` marker, and skips forward when one
case exceeds its own LTP timeout without returning to the runner.
"""

from __future__ import annotations

import argparse
import os
import re
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path


CASE_RE = re.compile(r"RUN LTP CASE ([^ :]+)")
LTP_TIMEOUT_RE = re.compile(r"Timeout per run is (\d+)h (\d+)m (\d+)s")


def run_capture(cmd: list[str], cwd: Path) -> str:
    return subprocess.check_output(cmd, cwd=cwd, text=True)


def load_cases(repo: Path) -> list[str]:
    out = run_capture(["python3", "tools/ltp-batches.py", "--batch", "all", "--csv"], repo)
    return [case for case in out.strip().split(",") if case]


def kill_process_group(proc: subprocess.Popen[str]) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait(timeout=5)


def parse_ltp_timeout(line: str) -> int | None:
    match = LTP_TIMEOUT_RE.search(line)
    if not match:
        return None
    hours, minutes, seconds = (int(part) for part in match.groups())
    return hours * 3600 + minutes * 60 + seconds


def append_file(dst: Path, src: Path, header: str) -> None:
    if not src.exists():
        return
    with dst.open("a", encoding="utf-8", errors="replace") as out:
        out.write(f"\n===== {header} =====\n")
        out.write(src.read_text(encoding="utf-8", errors="replace"))
        out.write("\n")


def run_chunk(
    repo: Path,
    cases: list[str],
    chunk_id: int,
    out_dir: Path,
    default_timeout: int,
    grace: int,
) -> tuple[bool, str | None, float, list[str]]:
    cmd = [
        "make",
        "oscomp-local-rv64-ltp-musl",
        f"OSCOMP_LTP={','.join(cases)}",
    ]
    log_path = out_dir / f"chunk-{chunk_id:04d}-{'_'.join(cases[:2])}.stdout.log"
    start = time.monotonic()
    active_case: str | None = None
    active_start = start
    active_timeout = default_timeout
    timed_out_case: str | None = None
    seen_cases: list[str] = []

    with log_path.open("w", encoding="utf-8", errors="replace") as log:
        proc = subprocess.Popen(
            cmd,
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        assert proc.stdout is not None
        sel = selectors.DefaultSelector()
        sel.register(proc.stdout, selectors.EVENT_READ)
        while proc.poll() is None:
            now = time.monotonic()
            if active_case is not None and now - active_start > active_timeout + grace:
                timed_out_case = active_case
                msg = (
                    f"\nWATCHDOG LTP CASE {active_case} exceeded "
                    f"{active_timeout}+{grace}s; terminating chunk\n"
                )
                print(msg, end="", flush=True)
                log.write(msg)
                kill_process_group(proc)
                break
            events = sel.select(timeout=1)
            for key, _ in events:
                line = key.fileobj.readline()
                if not line:
                    continue
                print(line, end="", flush=True)
                log.write(line)
                match = CASE_RE.search(line)
                if match:
                    active_case = match.group(1)
                    seen_cases.append(active_case)
                    active_start = time.monotonic()
                    active_timeout = default_timeout
                    continue
                if "SKIP LTP CASE " in line:
                    parts = line.strip().split()
                    if len(parts) >= 4:
                        seen_cases.append(parts[3])
                    continue
                parsed_timeout = parse_ltp_timeout(line)
                if parsed_timeout is not None:
                    active_timeout = parsed_timeout
        if proc.poll() is None:
            kill_process_group(proc)
        for line in proc.stdout:
            print(line, end="", flush=True)
            log.write(line)

    elapsed = time.monotonic() - start
    serial = repo / "target" / "oscomp" / "os_serial_out_rv.txt"
    append_file(out_dir / "combined-serial.txt", serial, f"chunk {chunk_id}")
    return timed_out_case is None and proc.returncode == 0, timed_out_case, elapsed, seen_cases


def make_chunk(cases: list[str], start: int, max_cases: int, max_group_chars: int) -> list[str]:
    chunk: list[str] = []
    chars = 0
    for case in cases[start:]:
        next_chars = chars + len(case) + (1 if chunk else 0)
        if chunk and (len(chunk) >= max_cases or next_chars > max_group_chars):
            break
        chunk.append(case)
        chars = next_chars
    return chunk


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--chunk-size", type=int, default=50)
    parser.add_argument("--max-group-chars", type=int, default=420)
    parser.add_argument("--default-timeout", type=int, default=75)
    parser.add_argument("--grace", type=int, default=15)
    parser.add_argument("--start-after", default="")
    parser.add_argument("--max-chunks", type=int, default=0)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    out_dir = repo / "target" / "oscomp" / "ltp-all-watchdog"
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "combined-serial.txt").write_text("", encoding="utf-8")
    skip_log = out_dir / "hard-timeout-skips.txt"

    cases = load_cases(repo)
    start_index = 0
    if args.start_after:
        try:
            start_index = cases.index(args.start_after) + 1
        except ValueError:
            raise SystemExit(f"start-after case not found: {args.start_after}")

    index = start_index
    chunk_id = 0
    slow_chunks: list[tuple[int, str, float]] = []
    hard_skips: list[str] = []
    while index < len(cases):
        if args.max_chunks and chunk_id >= args.max_chunks:
            break
        chunk_id += 1
        chunk = make_chunk(cases, index, args.chunk_size, args.max_group_chars)
        print(
            f"\n===== LTP all chunk {chunk_id}: index {index + 1}/{len(cases)} "
            f"{chunk[0]}..{chunk[-1]} =====",
            flush=True,
        )
        ok, timed_out_case, elapsed, seen_cases = run_chunk(
            repo, chunk, chunk_id, out_dir, args.default_timeout, args.grace
        )
        slow_chunks.append((chunk_id, f"{chunk[0]}..{chunk[-1]}", elapsed))
        if timed_out_case is not None:
            hard_skips.append(timed_out_case)
            with skip_log.open("a", encoding="utf-8") as f:
                f.write(timed_out_case + "\n")
            try:
                index = cases.index(timed_out_case, index) + 1
            except ValueError:
                index += args.chunk_size
            continue
        if chunk[-1] not in seen_cases:
            known_seen = [case for case in seen_cases if case in cases]
            if known_seen:
                last_seen = known_seen[-1]
                print(
                    f"chunk {chunk_id} did not reach expected last case {chunk[-1]}; "
                    f"resuming after observed {last_seen}",
                    flush=True,
                )
                index = cases.index(last_seen, index) + 1
                continue
            print(
                f"chunk {chunk_id} produced no recognized case markers; advancing conservatively",
                flush=True,
            )
        index += len(chunk)
        if not ok:
            print(f"chunk {chunk_id} exited non-zero; continuing with next chunk", flush=True)

    combined = out_dir / "combined-serial.txt"
    judge = subprocess.run(
        ["python3", "tools/oscomp-judge.py", str(combined), "target/oscomp/testdata"],
        cwd=repo,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    (out_dir / "combined-judge.log").write_text(judge.stdout, encoding="utf-8", errors="replace")
    print("\n===== combined judge =====")
    print(judge.stdout)
    if hard_skips:
        print("===== hard timeout skips =====")
        for case in hard_skips:
            print(case)
    print("===== slowest chunks =====")
    for chunk_id, label, elapsed in sorted(slow_chunks, key=lambda item: item[2], reverse=True)[:10]:
        print(f"{chunk_id:04d} {elapsed:8.1f}s {label}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
