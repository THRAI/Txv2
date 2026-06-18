#!/usr/bin/env python3
"""Merge root re-measurement (results-root/) into judged-full.json and emit
summary of changes. Run regen-doc.py afterwards to rebuild the table."""
import json, os, re, subprocess

BASE = '/home/msp/learning/Txv2/target/ltp-full-sweep'
TD = '/home/msp/learning/Txv2/target/oscomp/testdata'
JUDGES = {'musl': f'{TD}/judge_ltp-musl.py', 'glibc': f'{TD}/judge_ltp-glibc.py'}

rows = json.load(open(f'{BASE}/judged-full.json'))
byname = {r['t']: r for r in rows}

durs = {}
for line in open(f'{BASE}/results-root/durations.txt'):
    p = line.split()
    if len(p) == 2: durs[p[0]] = float(p[1])

gains = []
runaway = []
for t, dur in durs.items():
    log = f'{BASE}/results-root/{t}.log'
    if not os.path.exists(log) or t not in byname: continue
    data = open(log, 'rb').read()
    # Skip runaway time-dependent counters (cgroup/memcg/oom/ksm loop and emit
    # thousands of Summary blocks; score is machine-speed noise, not a ceiling).
    if data.count(b'Summary:') > 5:
        runaway.append(t)
        continue
    sc = {}
    for lane, judge in JUDGES.items():
        out = subprocess.run(['python3', judge], input=data, capture_output=True).stdout
        try: sc[lane] = sum(e['score'] for e in json.loads(out))
        except Exception: sc[lane] = 0
    r = byname[t]
    r['rootdir'] = True  # reasons now come from results-root
    if sc['musl'] > r['musl'] or sc['glibc'] > r['glibc']:
        gains.append((t, r['musl'], sc['musl'], dur))
        r['musl'] = max(r['musl'], sc['musl'])
        r['glibc'] = max(r['glibc'], sc['glibc'])
        r['dur'] = dur
        r['rootfixed'] = True

json.dump(rows, open(f'{BASE}/judged-full.json', 'w'))
tm = sum(r['musl'] for r in rows); tg = sum(r['glibc'] for r in rows)
sc_n = sum(1 for r in rows if r['musl'] > 0 or r['glibc'] > 0)
gains.sort(key=lambda x: -(x[2] - x[1]))
print(f'root-rerun files: {len(durs)}  gained: {len(gains)}  runaway-skipped: {len(runaway)} -> {sorted(runaway)}')
print(f'NEW TOTALS: musl={tm} glibc={tg} scoring_files={sc_n}')
for t, old, new, dur in gains[:40]:
    print(f'  +{new-old:4d}  {t:32s} {old}->{new}  ({dur:.1f}s)')
