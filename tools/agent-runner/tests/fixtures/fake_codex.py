#!/usr/bin/env python3
from __future__ import annotations

import json
import pathlib
import sys


def main() -> int:
    args = sys.argv[1:]
    prompt = sys.stdin.read()
    final_path = pathlib.Path(args[args.index("--output-last-message") + 1])
    final_path.write_text(
        "\n".join(
            [
                "- changed files: none",
                "- commands run: fake codex smoke pass",
                "- blockers: none",
                "- next step: run a real status smoke with explicit approval",
                "",
            ]
        ),
        encoding="utf-8",
    )
    print(json.dumps({"event": "fake_codex", "prompt_bytes": len(prompt)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
