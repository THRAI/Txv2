#!/usr/bin/env python3
"""Retarget copied RV64 glibc executables to Alpine's musl loader."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import struct

GLIBC_INTERPRETER = b"/lib/ld-linux-riscv64-lp64d.so.1\0"
MUSL_INTERPRETER = b"/lib/ld-musl-riscv64.so.1\0"
PT_INTERP = 3
EM_RISCV = 243


class InterpreterPatchError(Exception):
    pass


def patch_file(path: Path) -> bool:
    with path.open("r+b") as stream:
        header = stream.read(64)
        if not header.startswith(b"\x7fELF"):
            return False
        if header[4:6] != b"\x02\x01":
            raise InterpreterPatchError(f"unsupported ELF format: {path}")
        if struct.unpack_from("<H", header, 18)[0] != EM_RISCV:
            return False
        phoff = struct.unpack_from("<Q", header, 32)[0]
        phentsize = struct.unpack_from("<H", header, 54)[0]
        phnum = struct.unpack_from("<H", header, 56)[0]
        if phentsize < 56:
            raise InterpreterPatchError(f"invalid ELF program headers: {path}")
        interp = None
        for index in range(phnum):
            stream.seek(phoff + index * phentsize)
            program_header = stream.read(phentsize)
            if len(program_header) != phentsize:
                raise InterpreterPatchError(f"truncated ELF program header: {path}")
            if struct.unpack_from("<I", program_header, 0)[0] == PT_INTERP:
                if interp is not None:
                    raise InterpreterPatchError(f"multiple PT_INTERP entries: {path}")
                interp = (
                    phoff + index * phentsize,
                    struct.unpack_from("<Q", program_header, 8)[0],
                    struct.unpack_from("<Q", program_header, 32)[0],
                    struct.unpack_from("<Q", program_header, 40)[0],
                )
        if interp is None:
            return False
        header_offset, offset, size, mem_size = interp
        stream.seek(offset)
        current = stream.read(size)
        if len(current) != size:
            raise InterpreterPatchError(f"truncated PT_INTERP value: {path}")
        if current == MUSL_INTERPRETER and mem_size == len(MUSL_INTERPRETER):
            return False
        if current != GLIBC_INTERPRETER or mem_size != len(GLIBC_INTERPRETER):
            raise InterpreterPatchError(f"unexpected PT_INTERP value in {path}: {current!r}")
        if size < len(MUSL_INTERPRETER):
            raise InterpreterPatchError(f"PT_INTERP is too short for musl: {path}")
        stream.seek(offset)
        stream.write(MUSL_INTERPRETER + b"\0" * (size - len(MUSL_INTERPRETER)))
        stream.seek(header_offset + 32)
        stream.write(struct.pack("<QQ", len(MUSL_INTERPRETER), len(MUSL_INTERPRETER)))
    return True


def patch_tree(root: Path) -> int:
    if not root.is_dir():
        raise InterpreterPatchError(f"missing toolchain root: {root}")
    patched = 0
    for path in sorted(root.rglob("*")):
        if not path.is_symlink() and path.is_file() and os.access(path, os.X_OK):
            patched += int(patch_file(path))
    return patched


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("toolchain_root", type=Path)
    parser.add_argument("--allow-noop", action="store_true")
    args = parser.parse_args()
    try:
        patched = patch_tree(args.toolchain_root)
        if not patched and not args.allow_noop:
            raise InterpreterPatchError(f"no RV64 glibc executables patched under {args.toolchain_root}")
        print(f"patched {patched} RV64 glibc interpreter(s)")
    except InterpreterPatchError as error:
        print(f"patch-riscv64-glibc-interpreter: {error}", file=os.sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
