# Network L3 Refactor Register

用于登记所有 `REFACTOR(net-l3)` 代码标记，防止后续遗忘。

## 使用规则

1. 每个待重构点必须对应唯一 `RFX-*` 编号。
2. 代码中必须出现同编号注释，例如：`[RFX-003]`。
3. 新增标记时，先登记本表，再提交代码。
4. 完成重构后，把状态从 `OPEN` 改为 `CLOSED`，并记录完成提交或日期。

## 检索命令

```bash
rg -n "REFACTOR\\(net-l3\\)|RFX-" crates/tx-subsystems/src
```

## 条目

| ID | Status | File | Purpose | Trigger | Keep-until | Notes |
|---|---|---|---|---|---|---|
| RFX-001 | OPEN | TBD | 预留 reactor 接线替换点模板 | reactor wait token ready | L1/L2 接线完成 | 首个模板条目，后续按实际位置更新 |
