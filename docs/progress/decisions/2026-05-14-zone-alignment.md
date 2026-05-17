# Zone 设计文档-vs-代码对齐 — 决议记录

**日期：** 2026-05-14
**类型：** 对齐审计 + 实施

## 背景

对比 `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`、
`docs/design/00_meta-framework/object_model_v2.md`、
`docs/ebr-zone/` 中的 Zone/Cap/EBR 设计规格与
`crates/tx-substrate/src/zone/` 的实际代码实现，识别差距并收口。

## 审计结论

核心机制高度对齐 — EBR epoch、五态槽生命周期（Free→Reserved→Live→Dead→Retiring→Free）、
Cap/Weak/IdentRef 引用层级、reserve/sign 两步提交、zone registry 引导清单均已正确实现。

**识别 6 个 gap：**

### GAP-1: IdentitySlot<T> 未实现（中高）

文档列出 `IdentitySlot<T>` 作为 identity table 条目的类型级护栏，代码中不存在。
→ 实现 `#[repr(transparent)] IdentitySlot<T>` super `Cap<T>`，迁移 `MountTableEntry`。

### GAP-2: Policy 标记类型为死代码（中）

`policy.rs` 定义 `RetainedEntityPolicy`/`PayloadPolicy`/`ObserverNodePolicy` 但无连接点。
→ 定义 `ZonePolicy` trait（`SUPPORTS_CAP`/`EBR_DELAYED_DROP`），`ZoneAllocated` 加 `type Policy`，`sign_for`/`sign` 编译期门禁。

### GAP-3: PayloadCap 无独立语义（低）

`PayloadCap::from_cap` 接受任意 `Cap<T>`，无法阻止 identity Cap 误包装。
→ 加 `T::Policy: IsPayloadPolicy` 编译期门禁。

### GAP-4: 文档签名用 Errno，代码用 ZoneError（低）

→ 文档两处修正。

### GAP-5: Adapter Zone 导出面不统一（低）

→ 18 个 adapter 统一为 25-type 规范面。

### GAP-6: Guard lifetime 无实际差异

`Guard<'g>` 泛型参数与 `epoch::guard()` 返回 `Guard<'static>` 一致。
无改动。

## 实施

共 75 个文件改动，0 破坏性变更，0 增量编译错误/警告。

新增类型和 trait：
- `ZonePolicy` trait（`SUPPORTS_CAP`/`EBR_DELAYED_DROP` 关联常量）
- `CapProducingPolicy` marker（RetainedEntity + PayloadPolicy）
- `IsPayloadPolicy` marker（仅 PayloadPolicy）
- `IdentitySlot<T>`（`#[repr(transparent)]` over `Cap<T>`，含标准 trait impls）

`ZoneAllocated` trait 变更：
- 新增 `type Policy: ZonePolicy = RetainedEntityPolicy<Self>`（关联类型默认值，nightly）

门禁：
- `sign_for`/`sign`: `where T::Policy: CapProducingPolicy`
- `PayloadCap::from_cap`: `where T: ZoneAllocated, T::Policy: IsPayloadPolicy`

## 决议

- Zone 设计文档与代码已在结构和语义层面对齐
- Policy 体系从死代码升级为编译期类型约束
- IdentitySlot 为 identity table 提供 obligation 类型护栏
- 后续子系统编写应统一通过 adapter 导入 zone 类型，不直连 `tx_substrate::zone`
