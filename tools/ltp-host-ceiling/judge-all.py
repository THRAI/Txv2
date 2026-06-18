#!/usr/bin/env python3
"""Score every colored sweep log with the REAL official judge scripts."""
import json, os, subprocess, sys

TD = '/home/msp/learning/Txv2/target/oscomp/testdata'
RES = sys.argv[1] if len(sys.argv) > 1 else "/tmp/ltp-net-sweep/results-color"
JUDGES = {'musl': f'{TD}/judge_ltp-musl.py', 'glibc': f'{TD}/judge_ltp-glibc.py'}

LIST = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'net-sweep-list.txt')
tests = sorted(open(LIST).read().split())
rows = []
for t in tests:
    log = f'{RES}/{t}.log'
    if not os.path.exists(log):
        continue
    data = open(log, 'rb').read()
    row = {'t': t}
    for lane, judge in JUDGES.items():
        out = subprocess.run(['python3', judge], input=data,
                             capture_output=True).stdout
        try:
            parsed = json.loads(out)
            row[lane] = sum(e['score'] for e in parsed)
        except Exception:
            row[lane] = None
    rows.append(row)

json.dump(rows, open('/tmp/ltp-net-sweep/judged-real.json', 'w'), indent=1)
tm = sum(r['musl'] or 0 for r in rows)
tg = sum(r['glibc'] or 0 for r in rows)
print(f'REAL-JUDGE totals: musl={tm}  glibc={tg}')
print(f'{"file":36s} {"musl":>5s} {"glibc":>6s}')
for r in rows:
    if (r['musl'] or 0) > 0 or (r['glibc'] or 0) > 0:
        mark = '' if r['musl'] == r['glibc'] else '   <-- lanes differ'
        print(f"{r['t']:36s} {r['musl']:5d} {r['glibc']:6d}{mark}")
