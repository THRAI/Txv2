#!/usr/bin/env python3
"""Score every full-sweep log with the REAL official judge scripts."""
import json, os, subprocess, sys

TD = '/home/msp/learning/Txv2/target/oscomp/testdata'
RES = '/home/msp/learning/Txv2/target/ltp-full-sweep/results'
JUDGES = {'musl': f'{TD}/judge_ltp-musl.py', 'glibc': f'{TD}/judge_ltp-glibc.py'}

durs = {}
if os.path.exists(f'{RES}/durations.txt'):
    for line in open(f'{RES}/durations.txt'):
        p = line.split()
        if len(p) == 2:
            durs[p[0]] = float(p[1])

tests = sorted(open('/tmp/full-sweep-list.txt').read().split())
rows = []
for t in tests:
    log = f'{RES}/{t}.log'
    if not os.path.exists(log):
        continue
    data = open(log, 'rb').read()
    row = {'t': t, 'dur': durs.get(t, -1)}
    for lane, judge in JUDGES.items():
        out = subprocess.run(['python3', judge], input=data,
                             capture_output=True).stdout
        try:
            parsed = json.loads(out)
            row[lane] = sum(e['score'] for e in parsed)
        except Exception:
            row[lane] = 0
    # sandbox-artifact tagging
    txt = data.decode('latin-1')
    tag = ''
    if 'Failed to acquire device' in txt: tag = 'needs-blockdev'
    elif 'Path is not writable: /proc/sys' in txt: tag = 'needs-global-sysctl'
    elif "exited with a non-zero code" in txt and 'modprobe' in txt: tag = 'needs-module'
    row['tag'] = tag
    rows.append(row)

json.dump(rows, open('/home/msp/learning/Txv2/target/ltp-full-sweep/judged-full.json', 'w'))
tm = sum(r['musl'] for r in rows); tg = sum(r['glibc'] for r in rows)
sc = [r for r in rows if r['musl'] > 0 or r['glibc'] > 0]
diff = [r for r in rows if r['musl'] != r['glibc']]
print(f'files={len(rows)} scoring={len(sc)} musl_total={tm} glibc_total={tg} lane_diffs={len(diff)}')
for r in diff[:20]:
    print(f"  DIFF {r['t']} musl={r['musl']} glibc={r['glibc']}")
