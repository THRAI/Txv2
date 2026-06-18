#!/usr/bin/env python3
"""Regenerate the full scoring table with a reason for every non-scoring row,
after merging the artifact-fix rerun results."""
import json, os, re, subprocess

BASE = '/home/msp/learning/Txv2/target/ltp-full-sweep'
TD = '/home/msp/learning/Txv2/target/oscomp/testdata'
JUDGES = {'musl': f'{TD}/judge_ltp-musl.py', 'glibc': f'{TD}/judge_ltp-glibc.py'}
ansi = re.compile(r'\x1b\[[0-9;]*m')

rows = json.load(open(f'{BASE}/judged-full.json'))
byname = {r['t']: r for r in rows}

# merge rerun (results-fix) — re-judge those logs
fixdur = {}
fixdir = f'{BASE}/results-fix'
if os.path.exists(f'{fixdir}/durations.txt'):
    for line in open(f'{fixdir}/durations.txt'):
        p = line.split()
        if len(p) == 2: fixdur[p[0]] = float(p[1])
improved = []
for t, dur in fixdur.items():
    log = f'{fixdir}/{t}.log'
    data = open(log, 'rb').read()
    sc = {}
    for lane, judge in JUDGES.items():
        out = subprocess.run(['python3', judge], input=data, capture_output=True).stdout
        try: sc[lane] = sum(e['score'] for e in json.loads(out))
        except Exception: sc[lane] = 0
    r = byname[t]
    r['logdir'] = fixdir  # rerun log is authoritative for reasons
    if sc['musl'] > r['musl'] or sc['glibc'] > r['glibc']:
        improved.append((t, r['musl'], sc['musl']))
        r['musl'], r['glibc'], r['dur'] = sc['musl'], sc['glibc'], dur
        r['fixed'] = True

def reason_for(r):
    logdir = r.get('logdir', f'{BASE}/results')
    try: txt = ansi.sub('', open(f"{logdir}/{r['t']}.log", errors='replace').read())
    except Exception: return '日志缺失'
    m = re.search(r'(TBROK|TCONF)\s*:\s*(.*)', txt)
    first = m.group(2).strip()[:90] if m else ''
    low = first.lower()
    if 'failed to acquire device' in low or 'free loop device' in low:
        return '沙箱:需 loop 块设备(真 root 上限>0) — ' + first
    if re.search(r'set(e|re)?[ug]id\(.*EINVAL', first):
        return '沙箱:userns 单 uid 映射(真 root 上限>0) — ' + first
    if ('not writable' in low or 'eacces' in low) and ('/proc' in first or '/sys' in first or 'premake it' in low):
        return '沙箱:需写全局 /proc//sys/cgroup(真 root 上限>0) — ' + first
    if 'eperm' in low or 'eacces' in low:
        return '沙箱:特权操作被 userns 挡(真 root 可复测) — ' + first
    if 'command rsh not found' in low:
        return '结构性 0:legacy 双机测试,需 rsh+远端主机(镜像无 rsh,评测单机)'
    if 'not found' in low:
        return '镜像缺命令 — ' + first
    if 'must call tst_run' in low:
        return '库文件被当测试执行'
    if 'requires libaio' in low or 'requires libnuma' in low:
        return '宿主缺开发库(已补建重测)'
    if 'numa node' in low or 'numa memory nodes' in low:
        return '需≥2 NUMA 节点(宿主单节点;评测 QEMU 大概率同样 TCONF) — ' + first
    if 'kernel config' in low:
        return '内核配置条件 — ' + first
    if first:
        return first
    if 'TPASS' in txt:
        return 'legacy 框架:有 TPASS 行但两个官方 judge 均不认(无 Summary/着色格式不符)'
    if 'TFAIL' in txt:
        return '全部用例 TFAIL(部分为沙箱特权问题,需逐个看)'
    return '无 LTP 输出(辅助程序/数据文件/静默 legacy — 官方同样 0)'

sc = sorted([r for r in rows if r['musl'] > 0 or r['glibc'] > 0],
            key=lambda r: (-max(r['musl'], r['glibc']), r['t']))
ns = sorted([r for r in rows if r['musl'] == 0 and r['glibc'] == 0], key=lambda r: r['t'])
tm = sum(r['musl'] for r in rows); tg = sum(r['glibc'] for r in rows)

CAVEAT_MATRIX = {
 'splice07': '431→真 root Linux ≈600-667(fd 类型²,沙箱缺 4 类)',
 'readahead01': '沙箱下限,真 root ≈22-26', 'accept03': '沙箱下限,真 root ≈23-26'}

L = []
L.append(f"""# LTP 全量官方计分总表（宿主机 Linux 实测·v2 修订版）— 2026-06-10

v2 修订：(1) 每个不算分文件都给出原因；(2) 修复三类沙箱伪影并重测——
组播路由缺失(accept02)、宿主缺 libaio/libnuma(47 个文件补建重编)；
(3) tst_fd 矩阵测试标注为下限。方法见 v1 头部说明/`tools/ltp-host-ceiling/`。

## 总计

| 指标 | 值 |
|---|---|
| 实测文件 | 2821（镜像 2822，`prctl04` 非 LTP 20240930 产物，无法构建） |
| **算分文件** | **{len(sc)}** |
| **musl judge 总分** | **{tm}** |
| **glibc judge 总分** | **{tg}** |
| 沙箱测不准类（真 root 上限>0，表内逐行已标） | 需 loop 块设备 ~146；userns 单 uid 映射(setuid 类) ~91；需写全局 /proc//sys/cgroup ~100；其余 EPERM 特权类若干 |
| legacy 框架（官方两 judge 均 0，不可救） | ~257 |
| 算分文件中 Summary 带 skipped>0（该行分数为下限） | 24（大头已标注：splice07 skip88、select03 skip16、accept03/readahead01 skip4…） |

⚠️ tst_fd 矩阵测试（splice07/readahead01/accept03）为沙箱下限：
splice07 真 root ≈600-667（fd 可创建类型数平方；stub 化 bpf/perf_event_open/
fanotify_init/userfaultfd/fsopen/memfd_secret 等可平方级涨分）。

## 算分测试（{len(sc)} 个，按分数降序）

| 测试名 | 算分 | 真实分数 | Linux 耗时 |
|---|---|---|---|
""")
for r in sc:
    score = str(r['musl']) if r['musl'] == r['glibc'] else f"{r['musl']}(glibc:{r['glibc']})"
    if r['t'] in CAVEAT_MATRIX: score += f"⚠️{CAVEAT_MATRIX[r['t']]}"
    if r.get('fixed'): score += '（伪影修复后重测）'
    L.append(f"| `{r['t']}` | ✅ | {score} | {r['dur']:.1f}s |\n")
L.append(f"\n## 不算分测试（{len(ns)} 个，字母序，全部给出原因）\n\n| 测试名 | 算分 | 原因 |\n|---|---|---|\n")
for r in ns:
    L.append(f"| `{r['t']}` | ❌ | {reason_for(r)} |\n")
L.append(f"\n## 合计\n\n- **算分总计：musl 口径 {tm} 分 / glibc 口径 {tg} 分（{len(sc)} 个文件）。**\n"
         f"- 伪影修复重测后提分的文件：{len(improved)} 个。\n"
         f"- 原始数据：results/(主扫) + results-fix/(重测) + judged-full.json。\n")
open('/home/msp/learning/Txv2/msp/ltp-full-official-scoring-table-2026-06-10-zh.md', 'w').write(''.join(L))
json.dump(rows, open(f'{BASE}/judged-full.json', 'w'))
print('improved files:', improved)
print(f'totals: musl={tm} glibc={tg} scoring={len(sc)}')
