# LTP 新/老测试:输出、代码与评分

判分脚本只认测试自报的 **`Summary: passed N` 块**。新框架打这个块、老框架不打——根本原因是两代框架的**实现**不同(不是输出格式碰巧不同)。

---

## 1. 新测试(tst_test 框架):有 Summary

### 测试源码(注册 `struct tst_test`)
```c
#include "tst_test.h"
static void run(void) {
    tst_res(TPASS, "...");          // 每个断言
}
static struct tst_test test = {     // ★ 注册新框架,全局指针 tst_test 非空
    .test_all = run,
};
```

### 框架输出代码(LTP `lib/tst_res.c` + `lib/tst_test.c`)
```c
// tst_res_():每个 TPASS 累加到【共享内存】的 results 结构
//   results = &ipc->results;  // ipc 由 setup_ipc() 用 mmap 映射成共享内存
//   → 所以 fork 出来的子进程的结果也汇总进同一个 results->passed

// 测试结束时 do_exit() 打印 Summary 块:
fprintf(stderr, "\nSummary:\n");
fprintf(stderr, "passed   %d\n", results->passed);
fprintf(stderr, "failed   %d\n", results->failed);
fprintf(stderr, "broken   %d\n", results->broken);
fprintf(stderr, "skipped  %d\n", results->skipped);
fprintf(stderr, "warnings %d\n", results->warnings);
```

### 实际输出
```
writev01.c:124: TPASS: invalid iov_len, expected: -1 (EINVAL), got: -1 (EINVAL)
... (共 6 条 TPASS)
Summary:
passed   6
failed   0
broken   0
skipped  0
warnings 0
```

---

## 2. 老测试(tst_resm 框架):无 Summary

### 测试源码(plain main,不注册 `struct tst_test`)
```c
#include "test.h"               // 老头文件
char *TCID = "abs01";
int TST_TOTAL = 1;
int main(void) {
    tst_resm(TPASS, "...");      // 每个断言,直接打印
    tst_exit();                  // ★ 只清理退出
}
```

### 框架代码(LTP `lib/tst_res.c`)
```c
// 结果函数按"是否注册了 struct tst_test"分流:
void tst_resm_(..., int ttype, ...) {
    if (tst_test) tst_res_(...);    // 新框架(走上面第 1 节)
    else          tst_res__(...);   // 老框架(本测试走这里)
}

// 老框架 tst_res__():只累加一个【局部】计数器,立即打印每行
if (ttype_result == TPASS)
    passed_cnt++;                   // 局部变量,非共享内存,无人汇总

// 老框架 tst_exit():只做清理,【没有任何 Summary 打印逻辑】
```

### 实际输出
```
abs01       1  TPASS  :  ...
abs01       2  TPASS  :  ...
（结束,没有 Summary: 块)
```

---

## 3. 判分代码(`testdata/judge_ltp-musl.py`)
```python
def parse_ltp_log(content):
    result = {}; current_case = None; in_summary = False
    for line in content.split('\n'):
        s = line.strip()
        if s.startswith('RUN LTP CASE'):                    # 开始一个 case
            current_case = s.split()[-1]
            summary_data = {'passed':0,'failed':0,'broken':0,'skipped':0,'warnings':0,'all':0}
            in_summary = False
        elif current_case and s.startswith(f'FAIL LTP CASE {current_case}'):
            success = summary_data['passed']                # ★ 单 case 分 = Summary.passed
            result[current_case] = {**summary_data, 'success': success}
            current_case = None
        elif current_case:
            if s == 'Summary:': in_summary = True; continue # 只有进入 Summary 块后
            if in_summary:
                if not s: in_summary = False; continue
                p = s.split()
                if len(p) >= 2 and p[0] in ['passed','failed','broken','skipped','warnings']:
                    summary_data[p[0]] += int(p[1]); summary_data['all'] += int(p[1])
    return result
```
judge **只从 `Summary:` 块取 `passed`**;不解析 `TPASS` 行、不看退出码。没出现 `Summary:` → `passed` 恒为 0。

---

## 4. 为什么老测试拿不到分

| | 新框架 tst_test | 老框架 tst_resm |
|---|---|---|
| 测试源码 | 注册 `struct tst_test` | plain `main` + `tst_exit()` |
| 结果计数 | `results->passed`,**共享内存** | 局部 `passed_cnt` |
| 退出时 | `do_exit()` **打印 Summary 块** | `tst_exit()` 只清理,**不打 Summary** |
| judge 取分 | 从 Summary 取 `passed` | 取不到 → **0** |

**根本原因链:**
1. 老框架的测试源码不注册 `struct tst_test` → 走 `tst_res__` 老路径。
2. 老路径只有局部计数器、退出时不打印 `Summary:` 块(框架根本没这段代码)。
3. judge 只认 `Summary: passed N`,找不到就取 0。

→ 所以**老测试即使全部 TPASS、退出码 0,judge 也判 0 分**,这是框架实现决定的、结构上不可得分。提分只在新框架(tst_test)用例上有意义。
