#!/usr/bin/env python3
import os, re, json, sys

RES = '/tmp/ltp-net-sweep/results'
durs = {}
for line in open(f'{RES}/durations.txt'):
    parts = line.split()
    if len(parts) == 2:
        durs[parts[0]] = float(parts[1])

manifest = json.load(open('/tmp/manifest-present.json'))

rows = []
for t in sorted(open('/tmp/ltp-net-sweep/sweep-list.txt').read().split()):
    log = f'{RES}/{t}.log'
    if not os.path.exists(log):
        continue
    text = open(log, errors='replace').read()
    summary = {'passed':0,'failed':0,'broken':0,'skipped':0,'warnings':0}
    # judge logic: accumulate every Summary block
    in_sum = False
    for line in text.splitlines():
        s = line.strip()
        if s == 'Summary:':
            in_sum = True; continue
        if in_sum:
            if not s: in_sum = False; continue
            p = s.split()
            if len(p) >= 2 and p[0] in summary:
                try: summary[p[0]] += int(p[1])
                except ValueError: pass
    # first interesting verdict line for the reason column
    reason = ''
    for pat in ('TCONF:', 'TBROK:', 'TFAIL:'):
        m = re.search(r'^.*' + pat + r'.*$', text, re.M)
        if m:
            reason = m.group(0).strip()[:160]; break
    has_summary = 'Summary:' in text
    rc = ''
    m = re.search(rf'^FAIL LTP CASE {re.escape(t)} : (\d+)', text, re.M)
    if m: rc = m.group(1)
    fams = manifest.get(t, [])
    rows.append({
        't': t, 'fam': ','.join(fams) if fams else '(helper/no-manifest)',
        'score': summary['passed'], **summary,
        'dur': durs.get(t, -1), 'rc': rc,
        'has_summary': has_summary, 'reason': reason,
    })

json.dump(rows, open('/tmp/ltp-net-sweep/parsed.json','w'), indent=1)
score_total = sum(r['score'] for r in rows)
scorers = [r for r in rows if r['score'] > 0]
print(f"files: {len(rows)}  | files with score>0 on Linux: {len(scorers)}  | total Linux-ceiling points: {score_total}")
print()
fam_agg = {}
for r in rows:
    for f in (r['fam'].split(',') if r['fam'] != '(helper/no-manifest)' else [r['fam']]):
        a = fam_agg.setdefault(f, [0,0,0])
        a[0] += 1
        a[1] += r['score']
        a[2] += 1 if r['score']>0 else 0
print(f"{'family':28s} {'files':>5s} {'scoring-files':>13s} {'points':>6s}")
for f in sorted(fam_agg):
    a = fam_agg[f]
    print(f"{f:28s} {a[0]:5d} {a[2]:13d} {a[1]:6d}")
