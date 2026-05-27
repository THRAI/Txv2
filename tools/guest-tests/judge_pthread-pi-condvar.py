#!/usr/bin/env python3
"""Judge for the branch-local pthread PI-condvar guest test."""

import json
import sys


content = sys.stdin.read()
passed = (
    "#### TX GUEST TEST START pthread-pi-condvar ####" in content
    and "PASS pthread-pi-condvar" in content
    and "#### TX GUEST TEST END pthread-pi-condvar ####" in content
    and "TX guest wrapper: ./pthread_pi_condvar returned 0" in content
)

print(
    json.dumps(
        [
            {
                "name": "pthread_pi_condvar",
                "pass": 1 if passed else 0,
                "all": 1,
                "score": 1 if passed else 0,
            }
        ]
    )
)
