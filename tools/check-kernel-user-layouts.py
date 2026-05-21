#!/usr/bin/env python3
"""Compare kernel-to-user layout candidates with pinned musl headers.

The Rust side opts in through `KernelToUserLayout` descriptors in tx-shims.
This script asks tx-shims to dump those descriptors, compiles a generated C
probe against `external/musl`, and compares `sizeof`, `_Alignof`, and
`offsetof` for every marked field. The companion candidate registry is the
redlight: every musl-facing kernel/user struct surface must be marked as
checked, prefix-checked, manually byte-checked, deferred, or excluded with a
reason. For registered candidates backed by local Rust structs, the source lint
also rejects missing `KernelToUserLayout` markers, so new byte-image structs
cannot bypass the registry by accident while unrelated Linux UAPI PODs stay out
of the musl redlight.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


DEFAULT_ARCHES = ("riscv64", "loongarch64")
ACCEPTED_STATUSES = {"checked", "prefix", "manual", "deferred", "excluded"}
PROBED_STATUSES = {"checked", "prefix", "manual"}
SOURCE_LINT_PATHS = (
    Path("crates/tx-shims/src/linux_syscall"),
    Path("crates/tx-subsystems/src/tty/structure/termios.rs"),
    Path("crates/tx-subsystems/src/tty/structure/winsize.rs"),
)


def run(
    argv: list[str],
    *,
    cwd: Path,
    capture: bool = False,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            argv,
            cwd=cwd,
            input=input_text,
            text=True,
            stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.PIPE if capture else None,
            check=True,
        )
    except subprocess.CalledProcessError as error:
        details = ""
        if error.stdout:
            details += error.stdout
        if error.stderr:
            details += error.stderr
        if details:
            raise SystemExit(details.rstrip()) from error
        raise


def load_rust_layouts(root: Path) -> dict[str, Any]:
    output = run(
        [
            "cargo",
            "run",
            "-q",
            "-p",
            "tx-shims",
            "--bin",
            "dump-kernel-user-layouts",
        ],
        cwd=root,
        capture=True,
    ).stdout
    return json.loads(output)


def shell_quote(path: Path) -> str:
    return str(path).replace("'", "'\\''")


def build_alltypes(root: Path, arch: str, include_dir: Path) -> None:
    musl = root / "external" / "musl"
    arch_alltypes = musl / "arch" / arch / "bits" / "alltypes.h.in"
    generic_alltypes = musl / "include" / "alltypes.h.in"
    mkalltypes = musl / "tools" / "mkalltypes.sed"
    if not arch_alltypes.exists():
        raise SystemExit(f"musl arch not found: {arch_alltypes}")
    (include_dir / "bits").mkdir(parents=True, exist_ok=True)
    script = (
        f"sed -f '{shell_quote(mkalltypes)}' "
        f"'{shell_quote(arch_alltypes)}' '{shell_quote(generic_alltypes)}'"
    )
    alltypes = run(["sh", "-c", script], cwd=root, capture=True).stdout
    (include_dir / "bits" / "alltypes.h").write_text(alltypes)


def c_string(value: str) -> str:
    return json.dumps(value)


def probe_subjects(layouts: dict[str, Any]) -> list[dict[str, Any]]:
    subjects: list[dict[str, Any]] = []
    candidates = layouts.get("candidates", [])
    if not candidates:
        for layout in layouts["layouts"]:
            subject = dict(layout)
            subject["probe_name"] = layout["rust_type"]
            subject["status"] = "checked"
            subjects.append(subject)
        return subjects
    for candidate in candidates:
        if candidate.get("status") not in PROBED_STATUSES:
            continue
        if not candidate.get("musl_header") or not candidate.get("musl_type"):
            continue
        subject = dict(candidate)
        subject["probe_name"] = candidate["name"]
        subjects.append(subject)
    return subjects


def generate_probe(layouts: dict[str, Any]) -> str:
    subjects = probe_subjects(layouts)
    headers = sorted({subject["musl_header"] for subject in subjects})
    lines = [
        "#include <stddef.h>",
        "#include <stdint.h>",
        "#include <stdio.h>",
    ]
    lines.extend(f"#include <{header}>" for header in headers)
    lines.extend(
        [
            "",
            "int main(void) {",
            "  uint32_t index = 0;",
            "  (void)index;",
        ]
    )
    for subject in subjects:
        probe_name = subject["probe_name"]
        musl_type = subject["musl_type"]
        lines.append(
            "  printf(\"L\\t%u\\t%s\\t%zu\\t%zu\\n\", "
            f"index, {c_string(probe_name)}, sizeof({musl_type}), _Alignof({musl_type}));"
        )
        for field in subject["fields"]:
            lines.append(
                "  printf(\"F\\t%u\\t%s\\t%zu\\n\", "
                f"index, {c_string(field['rust'])}, offsetof({musl_type}, {field['musl']}));"
            )
        lines.append("  index++;")
    lines.append("  return 0;")
    lines.append("}")
    return "\n".join(lines)


def compile_and_run_probe(root: Path, arch: str, layouts: dict[str, Any], cc: str) -> dict[str, Any]:
    musl = root / "external" / "musl"
    if not musl.exists():
        raise SystemExit("external/musl is missing; cannot extract musl layouts")
    with tempfile.TemporaryDirectory(prefix=f"tx-musl-layout-{arch}-") as tmp:
        tmp_path = Path(tmp)
        generated_include = tmp_path / "include"
        build_alltypes(root, arch, generated_include)
        probe_c = tmp_path / "probe.c"
        probe_bin = tmp_path / "probe"
        probe_c.write_text(generate_probe(layouts))
        argv = [
            cc,
            "-std=c11",
            "-D_GNU_SOURCE",
            f"-I{generated_include}",
            f"-I{musl / 'arch' / arch}",
            f"-I{musl / 'arch' / 'generic'}",
            f"-I{musl / 'include'}",
            str(probe_c),
            "-o",
            str(probe_bin),
        ]
        run(argv, cwd=root, capture=True)
        raw = run([str(probe_bin)], cwd=root, capture=True).stdout
    return parse_probe_output(raw)


def parse_probe_output(raw: str) -> dict[int, dict[str, Any]]:
    out: dict[int, dict[str, Any]] = {}
    for line in raw.splitlines():
        parts = line.split("\t")
        if not parts:
            continue
        if parts[0] == "L" and len(parts) == 5:
            index = int(parts[1])
            out[index] = {
                "rust_type": parts[2],
                "size": int(parts[3]),
                "align": int(parts[4]),
                "fields": {},
            }
        elif parts[0] == "F" and len(parts) == 4:
            index = int(parts[1])
            out.setdefault(index, {"fields": {}})["fields"][parts[2]] = int(parts[3])
        else:
            raise ValueError(f"unexpected probe line: {line!r}")
    return out


def iter_source_lint_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for rel in SOURCE_LINT_PATHS:
        path = root / rel
        if path.is_dir():
            files.extend(path.rglob("*.rs"))
        elif path.exists():
            files.append(path)
    return sorted(
        file
        for file in files
        if "/tests/" not in file.as_posix()
        and file.name != "tests.rs"
        and "/src/bin/" not in file.as_posix()
    )


def source_marker_facts(root: Path) -> tuple[dict[str, list[str]], set[str]]:
    repr_c_structs: dict[str, list[str]] = {}
    marker_impls: set[str] = set()
    repr_c_struct_re = re.compile(
        r"#\s*\[\s*repr\s*\(\s*C\s*\)\s*\]\s*"
        r"(?:#\s*\[[^\]]*\]\s*)*"
        r"(?:pub(?:\s*\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
        re.MULTILINE,
    )
    marker_impl_re = re.compile(
        r"impl\s+KernelToUserLayout\s+for\s+([A-Za-z_][A-Za-z0-9_]*)"
    )
    marker_macro_re = re.compile(
        r"marked_kernel_user_layout!\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*,",
        re.MULTILINE,
    )
    for file in iter_source_lint_files(root):
        text = file.read_text()
        rel = file.relative_to(root).as_posix()
        for match in repr_c_struct_re.finditer(text):
            name = match.group(1)
            line = text.count("\n", 0, match.start(1)) + 1
            repr_c_structs.setdefault(name, []).append(f"{rel}:{line}")
        marker_impls.update(marker_impl_re.findall(text))
        marker_impls.update(marker_macro_re.findall(text))
    return repr_c_structs, marker_impls


def required_marker_types(rust: dict[str, Any]) -> set[str]:
    return {
        candidate["rust_type"]
        for candidate in rust.get("candidates", [])
        if candidate.get("rust_type")
    }


def check_source_marker_coverage(root: Path, rust: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    required = required_marker_types(rust)
    repr_c_structs, marker_impls = source_marker_facts(root)
    for name in sorted(required):
        locations = repr_c_structs.get(name, [])
        if not locations:
            errors.append(
                f"{name}: registered kernel/user ABI rust_type was not found in source lint paths"
            )
            continue
        if len(locations) > 1:
            errors.append(
                f"{name}: duplicate registered #[repr(C)] kernel/user ABI rust_type at "
                f"{', '.join(locations)}; use a shared marked type or a unique name"
            )
            continue
        if name not in marker_impls:
            errors.append(
                f"{locations[0]}: {name}: registered #[repr(C)] kernel/user ABI rust_type "
                "must implement KernelToUserLayout"
            )
    return errors


def check_candidate_coverage(rust: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    seen: set[str] = set()
    for candidate in rust.get("candidates", []):
        name = candidate.get("name", "<unnamed>")
        if name in seen:
            errors.append(f"{name}: duplicate kernel-user candidate")
        seen.add(name)
        status = candidate.get("status")
        if status not in ACCEPTED_STATUSES:
            errors.append(
                f"{name}: candidate status must be checked, prefix, manual, deferred, or excluded (got {status})"
            )
            continue
        if status in {"deferred", "excluded"} and not candidate.get("reason"):
            errors.append(f"{name}: {status} candidate needs a reason")
        if status in PROBED_STATUSES:
            for key in ("musl_header", "musl_type"):
                if not candidate.get(key):
                    errors.append(f"{name}: {status} candidate missing {key}")
            if candidate.get("size", 0) <= 0:
                errors.append(f"{name}: {status} candidate needs a positive size")
            if candidate.get("align", 0) < 0:
                errors.append(f"{name}: {status} candidate has negative align")
    if not rust.get("candidates"):
        errors.append("kernel-user candidate registry is empty")
    checked_candidate_keys = {
        (candidate.get("musl_header"), candidate.get("musl_type"))
        for candidate in rust.get("candidates", [])
        if candidate.get("status") == "checked"
    }
    for layout in rust.get("layouts", []):
        key = (layout.get("musl_header"), layout.get("musl_type"))
        if key not in checked_candidate_keys:
            errors.append(
                f"{layout.get('rust_type', '<unnamed layout>')}: full layout missing checked candidate"
            )
    return errors


def compare_arch(arch: str, rust: dict[str, Any], musl: dict[int, dict[str, Any]]) -> list[str]:
    errors: list[str] = []
    for index, layout in enumerate(probe_subjects(rust)):
        c_layout = musl.get(index)
        probe_name = layout["probe_name"]
        if c_layout is None:
            errors.append(f"{arch}:{probe_name}: missing musl probe record")
            continue
        if c_layout["rust_type"] != probe_name:
            errors.append(f"{arch}:{probe_name}: probe index returned {c_layout['rust_type']}")
        status = layout.get("status", "checked")
        if c_layout["size"] < layout["size"]:
            errors.append(
                f"{arch}:{probe_name}: size rust={layout['size']} musl={c_layout['size']}"
            )
        elif status == "checked" and c_layout["size"] != layout["size"]:
            errors.append(
                f"{arch}:{probe_name}: size rust={layout['size']} musl={c_layout['size']}"
            )
        if status == "checked" and c_layout["align"] != layout["align"]:
            errors.append(
                f"{arch}:{probe_name}: align rust={layout['align']} musl={c_layout['align']}"
            )
        elif layout.get("align", 0) and c_layout["align"] < layout["align"]:
            errors.append(
                f"{arch}:{probe_name}: align rust={layout['align']} musl={c_layout['align']}"
            )
        for field in layout["fields"]:
            rust_offset = field["offset"]
            c_offset = c_layout["fields"].get(field["rust"])
            if c_offset is None:
                errors.append(
                    f"{arch}:{probe_name}.{field['rust']}: missing musl field probe"
                )
            elif c_offset != rust_offset:
                errors.append(
                    f"{arch}:{probe_name}.{field['rust']}: "
                    f"offset rust={rust_offset} musl={c_offset} "
                    f"(musl field {field['musl']})"
                )
    return errors


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--arch",
        action="append",
        choices=DEFAULT_ARCHES,
        help="musl arch to probe; repeatable (default: riscv64 and loongarch64)",
    )
    parser.add_argument("--cc", default=os.environ.get("CC") or shutil.which("cc") or "cc")
    parser.add_argument(
        "--dump",
        action="store_true",
        help="print the Rust descriptor JSON and exit",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    root = Path(__file__).resolve().parents[1]
    rust = load_rust_layouts(root)
    if args.dump:
        print(json.dumps(rust, indent=2, sort_keys=True))
        return 0

    coverage_errors = check_candidate_coverage(rust)
    coverage_errors.extend(check_source_marker_coverage(root, rust))
    if coverage_errors:
        for error in coverage_errors:
            print(error, file=sys.stderr)
        return 1

    arches = args.arch or list(DEFAULT_ARCHES)
    all_errors: list[str] = []
    for arch in arches:
        musl = compile_and_run_probe(root, arch, rust, args.cc)
        errors = compare_arch(arch, rust, musl)
        if errors:
            all_errors.extend(errors)
        else:
            checked = len(probe_subjects(rust))
            candidates = len(rust.get("candidates", []))
            print(
                f"kernel-user layouts: {arch}: ok ({checked} checked probes, {candidates} candidates)"
            )

    if all_errors:
        for error in all_errors:
            print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
