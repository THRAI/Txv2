# tx-fat: FAT12/16/32 + VFAT 文件系统后端

<!-- txdoc:05-FILESYSTEM-FAT-VFAT-V1 -->

**Status.** v1 (2026-05-19). Draft plan。

**Purpose.** 定义 txKernel 的 FAT/VFAT 文件系统后端交付物、阶段划分、接口契约。FAT 族是 txKernel 第二个持久化文件系统后端（继 tx-ext4 之后），覆盖嵌入式和小容量介质的常见场景。

**Audience.** FAT 实现者、审查后端边界的 reviewer、后续阶段扩展的 agent。

**Companion documents.**

- [`BDEV_FS.md`](BDEV_FS.md) — 块设备文件系统层，FAT 挂载会走 bdev-fs 提供的 `BlockImage` 适配。
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) — `FsOps` trait，VFS walker/witness 规范。
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — `FsPageBacking` trait，`PageContainer` 模型。
- [`TX_EXT4_PLAN_v1_2.md`](TX_EXT4_PLAN_v1_2.md) — 架构参考：双 crate 模式、阶段划分结构、测试基础设施设计。
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) §3.5 — factoring/topology 轴。
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — `StepOutcome` 契约。

### Zone-derived type policy

<!-- txdoc:FAT-VFAT-ZONE-DERIVED-TYPE-POLICY-1 -->

tx-fat 是由 Mount 和 PageBacked 托管的文件系统后端。不引入新的用户可见 identity 实体：

| tx-fat 声明 | Public handle | Reclamation role |
|---|---|---|
| mounted fat instance | `MountPayload` evidence from MOUNT | filesystem-instance payload |
| file content cache | `Cap<PageContainer>` held by PageBacked/VFS | page-backed content entity |
| FAT table cache | values on mount payload (in-memory cache of FAT chains) | binding values, no zone |
| directory entries | values parsed from directory clusters | binding/materialization values, no zone |
| block device | `&'static BlockDeviceRegistration` or bdev-fs bridge | static/device-backed fact |

tx-fat 不引入 `Cap<RNode>` 缓存或原始 `Zone<T, Policy>` 选择。持久化对象标识使用 `FsObjectId`（编码为 cluster number + 目录内偏移），通过 VFS/PageBacked 解析。

---

## 1. Scope and non-goals

<!-- txdoc:FAT-VFAT-SCOPE-NON-GOALS-1 -->

### 1.1 In scope

<!-- txdoc:FAT-VFAT-IN-SCOPE-1 -->

- 一个 `tx-fat-format` crate（no_std）：FAT on-disk 结构解析、FAT 链遍历、目录枚举、VFAT LFN 解码。
- 一个 `tx-fat` crate：`impl FsOps for FatFsInstance<I>` + `impl FsPageBacking for FatFsInstance<I>`。
- FAT12、FAT16、FAT32 的读支持（包括子目录遍历）。
- VFAT 长文件名（LFN）—— Unicode UCS-2 → UTF-8 转换。
- 文件读取走 `FsPageBacking::fetch_page`。
- 初始挂载只读（`MS_RDONLY`），和 tx-ext4 phase 1 对齐。
- 目录读取（`readdir`），游标以 cluster + entry offset 表达。
- `lookup`、`load_inode_meta`、`materialise_rnode`、`read_link`（FAT 不支持 symlink，返回 ENOSYS）。

### 1.2 Out of scope for v1

<!-- txdoc:FAT-VFAT-OUT-OF-SCOPE-V1-1 -->

- 写操作（`create_inode`、`mkdir`、`unlink`、`rename`、`serialize_inode_meta`、`flush_page`、`truncate`）— 全部 ENOSYS 或 EROFS。
- FAT 表的写回和空闲簇分配。
- 目录项的创建/删除。
- exFAT 支持（exFAT 的 on-disk 结构与 FAT12/16/32 完全不同，独立处理）。
- 长文件名创建（只读 LFN 解码）。
- 大小写处理的高级策略（v1 返回 on-disk 原始大小写；VFAT LFN 保留原始大小写）。
- `fsync_file` — 只读挂载下无意义。

---

## 2. Architecture overview

<!-- txdoc:FAT-VFAT-ARCHITECTURE-OVERVIEW-1 -->

### 2.1 Crate topology

```
tx-fat-format (no_std)
  └─ on-disk structures, BPB parser, FAT chain walker,
     directory entry parser, LFN decoder, FatPager<I: BlockImage>
     ↓ BlockImage trait (same as tx-ext4-format)
tx-fat (kernel adapter)
  └─ FatFsInstance<I>: wraps FatPager<I> with locking + mount pin
     ├─ impl FsOps        (namespace.rs)
     └─ impl FsPageBacking (pager.rs)
     ↓ Arc<dyn FsOps> + Arc<dyn FsPageBacking>
tx-fs (bridge)
  └─ fat_bridge.rs: BlockDevice → BlockImage adapter
```

沿用 tx-ext4 的双 crate 模式。`BlockImage` trait 与 tx-ext4-format 的 `BlockImage` 语义等价（`read_block`、`write_block`、`total_blocks`），但 tx-fat-format 定义自己的副本以保持 crate 独立。

### 2.2 FsObjectId scheme

FAT 没有 inode 号。FAT 目录项通过 **(starting cluster, entry offset within directory)** 定位。v1 使用 FsObjectId 编码：

- 高 32 bits: starting cluster number (0 = root, FAT12/16 root 用特殊值)
- 低 32 bits: entry offset within the starting cluster (用于同名文件的重名消歧)

根目录特殊处理：FAT12/16 根目录在固定区域（无簇号），cluster number = 0xFFFFFFFF 标识；FAT32 根目录簇号从 BPB 读取。

### 2.3 InodeMeta 映射

FAT 目录项字段 → `InodeMeta`：

| FAT field | InodeMeta field | Note |
|---|---|---|
| DIR_Attr (byte 11) | mode | 目录→ `S_IFDIR | 0555`，普通文件→ `S_IFREG | 0444`（RO 挂载） |
| 文件大小 (bytes 28-31) | size | 直接映射 |
| DIR_WrtTime + DIR_WrtDate | mtime | 转换为 Unix epoch |
| DIR_WrtTime + DIR_WrtDate | ctime | 和 mtime 相同（FAT 无独立 ctime） |
| — | atime | 0（FAT 无访问时间） |
| — | uid / gid | 0 (root) |
| — | nlinks | 1（FAT 无链接计数） |
| — | blocks_512 | 文件大小 / 512 向上取整 |
| — | flags | 0 |

---

## 3. Interfaces

<!-- txdoc:FAT-VFAT-INTERFACES-1 -->

### 3.1 Traits implemented

| Trait | Location | Phase |
|---|---|---|
| `FsOps` | `tx-fat::namespace::impl FsOps for FatFsInstance<I>` | Phase 1 |
| `FsPageBacking` | `tx-fat::pager::impl FsPageBacking for FatFsInstance<I>` | Phase 1 |

### 3.2 Traits consumed

| Trait | Crate | Purpose |
|---|---|---|
| `BlockImage` | defined in `tx-fat-format` | block-level read/write for FAT pager |

在 `tx-fs` 桥接层：`BlockDevice` (from `tx-subsystems::device`) → `BlockImage` 适配，和 `tx_ext4_bridge.rs` 的 `BlockDeviceImage` 模式相同。

### 3.3 Value types

| Type | Source | Used in |
|---|---|---|
| `FsObjectId` | `tx_subsystems::vfs::structure` | `lookup`, `load_inode_meta`, `materialise_rnode`, `fetch_page`, `readdir` |
| `InodeMeta` | `tx_subsystems::vfs::structure` | `load_inode_meta`, `materialise_rnode` |
| `DirCursor` | `tx_subsystems::vfs::structure` | `readdir` |
| `DirEntry` | `tx_subsystems::vfs::structure` | `readdir` |
| `Credential` | `tx_subsystems::vfs::structure` | `create_inode`, `mkdir` (v1: unused, returns EROFS) |
| `Frame` | `tx_subsystems::page_backed` | `fetch_page` return |
| `Guard` | `tx_substrate::epoch` | all method guard parameters |
| `StepOutcome<T, NoProgress>` | `tx_substrate::step` | all return types |

### 3.4 Mount handshake

```rust
// tx-fat/src/mount.rs
pub struct MountedFat<I> {
    backend: Arc<FatFsInstance<I>>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

impl<I: BlockImage + Send + 'static> MountedFat<I> {
    pub fn fs_ops(&self) -> Arc<dyn FsOps>;
    pub fn fs_page_backing(&self) -> Arc<dyn FsPageBacking>;
    pub fn bind_mount_payload(&self, payload: &Cap<MountPayload>);
}

pub fn mount_fat_read_only<I: BlockImage + Send + 'static>(image: I) -> Result<MountedFat<I>, Errno>;
pub fn mount_fat_read_write<I: BlockImage + Send + 'static>(image: I) -> Result<MountedFat<I>, Errno>;
```

---

## 4. Phase breakdown

<!-- txdoc:FAT-VFAT-PHASE-BREAKDOWN-1 -->

### Phase 0: Format crate 基础

`tx-fat-format` — on-disk 结构 + BPB 解析 + FAT 链遍历。

- [ ] **T0.1** BPB 解析：FAT12/16/32 的 BIOS Parameter Block，确定 FAT 类型、簇大小、根目录位置。
- [ ] **T0.2** FAT 链遍历：给定起始簇号，沿 FAT 表读取簇链，收集所有簇号。
- [ ] **T0.3** 目录项解析：8.3 短文件名 + VFAT LFN 条目解码。Unicode UCS-2LE → UTF-8。
- [ ] **T0.4** `FatPager<I: BlockImage>` 初始化：读入 BPB，缓存 FAT 表（FAT12/16 全量缓存；FAT32 按需加载）。
- [ ] **T0.5** 目录遍历：从根目录开始，逐簇读取目录项，返回 `DirEntryLite` 迭代器。

### Phase 1: Kernel adapter — 只读 FsOps + FsPageBacking

`tx-fat` — 内核适配层。

- [ ] **T1.1** `FatFsInstance<I>` 结构：封装 `FatPager<I>` + mount pin + read-only flag。
- [ ] **T1.2** `FsOps::lookup`：在当前目录簇中按文件名匹配目录项。
- [ ] **T1.3** `FsOps::load_inode_meta`：从目录项构造 `InodeMeta`。
- [ ] **T1.4** `FsOps::readdir`：遍历目录簇，每次返回一个 `DirEntry` + 新游标。
- [ ] **T1.5** `FsPageBacking::fetch_page`：从 FAT 簇链中读取对应文件页。
- [ ] **T1.6** `FsOps::materialise_rnode`：创建 PageContainer + RNode。
- [ ] **T1.7** `FsOps::read_link` → ENOSYS（FAT 不支持符号链接）。
- [ ] **T1.8** 所有写方法：`create_inode`、`mkdir`、`unlink`、`rename`、`link`、`symlink`、`serialize_inode_meta`、`destroy_inode` → EROFS（RO 挂载）。
- [ ] **T1.9** `FsPageBacking::flush_page`、`truncate`、`fsync_file` → EROFS。

### Phase 2: tx-fs 桥接

- [ ] **T2.1** `fat_bridge.rs`：`BlockDevice` → `BlockImage` 适配（复用 tx_ext4_bridge.rs 模式）。
- [ ] **T2.2** 在 `tx-fs/src/lib.rs` 注册 fat 模块。

### Phase 3: 内核集成

- [ ] **T3.1** workspace members 注册 `tx-fat-format`、`tx-fat`。
- [ ] **T3.2** 内核挂载路径：fat 文件系统类型注册，mount 时通过 bdev-fs 获取 block device → 桥接 → `mount_fat_read_only` → 挂载。

### Phase 4 (future): 读写支持

- FAT 表写回 + 空闲簇分配
- 目录项创建/删除
- `flush_page` + `truncate` 实现
- 简单写操作（覆盖写，无扩展）

---

## 5. Module layout

<!-- txdoc:FAT-VFAT-MODULE-LAYOUT-1 -->

### tx-fat-format

```
tx-fat-format/
    Cargo.toml
    src/
        lib.rs           — pub mod ondisk, pager; re-export BlockImage, FatPager
        ondisk.rs        — BPB, DirEntry, LFNDirEntry, FatType enum
        pager.rs         — FatPager<I: BlockImage>: open, read_cluster, walk_fat_chain,
                            read_dir_entries (DirEntryLite iterator)
```

### tx-fat

```
tx-fat/
    Cargo.toml
    src/
        lib.rs           — pub mod mount, namespace, pager, adapter
        adapter.rs       — platform_adapter! for step_engine domain
        mount.rs         — MountedFat<I>, mount_fat_read_only, mount_fat_read_write
        namespace.rs     — impl FsOps for FatFsInstance<I>
        pager.rs         — impl FsPageBacking for FatFsInstance<I>
        read_backend.rs  — FatFsInstance<I>: open, with_pager, FsObjectId ←→ cluster conversions
```

---

## 6. Testing infrastructure

<!-- txdoc:FAT-VFAT-TESTING-INFRASTRUCTURE-1 -->

### 6.1 Host-runnable 测试 (Phase 0)

`tx-fat-format` 标准 `cargo test`。测试镜像通过 `build.rs` 在测试阶段生成（使用 `mkfs.vfat` 或 `dd` + 手工构造的小型 FAT 镜像），或使用预生成的二进制测试镜像提交到仓库。

测试覆盖：
- BPB 解析（FAT12 / FAT16 / FAT32 各一）
- FAT 链遍历（短链、跨簇链、坏簇标记 0xFFF7）
- 目录项解析（8.3、LFN、混合、已删除条目 0xE5）
- 边界条件（空目录、满簇链、损坏的 BPB → 错误返回）

### 6.2 内核集成测试 (Phase 1+)

qemu-system-riscv64 + virtio-blk。测试工具：
- 提供已知内容的 FAT 镜像。
- 启动内核，测试 runner 替代 init。
- 测试 runner 通过 syscall 读文件、列目录，收集结果。

### 6.3 互操作测试 (Phase 1+)

tx-kernel 挂载 FAT 镜像 → 读文件 → 验证内容和 Linux `mount` + `cat` / `ls` 一致。反过来：Linux 创建的文件，tx-kernel 能正确读取。

---

## 7. Risk register

<!-- txdoc:FAT-VFAT-RISK-REGISTER-1 -->

| Risk | Phase | Mitigation |
|---|---|---|
| VFAT LFN Unicode 转换错误导致文件名匹配失败 | 0 | 单元测试覆盖 UCS-2LE → UTF-8 往返；用 Linux 生成的已知 LFN 镜像验证 |
| FAT32 的 FAT 表太大，全量缓存内存不够 | 0 | Phase 0 对 FAT32 使用按需分页加载；FAT12/16 全量缓存无压力 |
| 8.3 短文件名大小写处理与 Linux 行为不一致 | 1 | Linux 的 FAT 驱动行为文档化；v1 返回 on-disk 原始大小写 |
| 簇链中坏簇标记 (0xFFF7) 未处理导致读取错误 | 0 | FAT 链遍历时检测并返回 EIO |
| 子目录 `..` 条目指向自身或父目录的簇号解析错误 | 1 | 目录遍历时显式处理 `.` 和 `..`；lookup 时跳过 |
| 大文件（> 4 GB）的 FAT32 大小字段溢出 | 0 | FAT32 目录项 4 字节 size 字段；v1 中 > 4 GB 文件返回 EFBIG |

---

## 8. Open decisions deferred to implementation start

<!-- txdoc:FAT-VFAT-OPEN-DECISIONS-DEFERRED-IMPLEMENTATION-START-1 -->

1. **BlockImage trait 共享 vs 独立定义。** `tx-ext4-format` 已有 `BlockImage` trait；tx-fat-format 是定义自己的副本还是依赖 `tx-ext4-format` 仅为其 trait？副本保持独立性但产生两个语义等价的 trait；共享引入不必要的 crate 依赖。决策：v1 定义独立副本；如未来统一，可提取到 `tx-fs` 或单独 crate。
2. **FAT 表缓存策略。** FAT12 (≤ 4084 簇) 和 FAT16 (≤ 65524 簇) 适合全量缓存（几 KB 到 128 KB）。FAT32 可能很大（数百万簇，每簇 4 字节 = 数 MB）。Phase 0 对 FAT32 使用 LRU 按需加载；阈值待定。
3. **长文件名 (LFN) 校验和。** LFN 条目包含 8.3 条目的校验和。是否在 lookup 时验证校验和匹配？不匹配时静默退回 8.3 还是报 EIO？决策：v1 验证校验和，不匹配时报 EIO（损坏的文件系统）。
4. **FAT 卷标签。** BPB 和根目录中可能有卷标条目。是否暴露为 `InodeMeta` 或 mount 信息的一部分？v1 忽略卷标。
5. **时间戳精度。** FAT 目录项时间精度为 2 秒（FAT16）或更粗。`InodeMeta` 的 nsec 字段置 0。

---

## 9. Success criterion, compressed

<!-- txdoc:FAT-VFAT-SUCCESS-CRITERION-COMPRESSED-1 -->

tx-kernel 在 qemu-virtio-blk 上以 FAT32 镜像为根文件系统启动。`ls`、`cat`、`cd` 能正确遍历 FAT32 目录树并显示文件内容。VFAT 长文件名正确渲染。和 Linux 的 `mount -t vfat` 交叉验证：两边看到的文件名和内容一致。

---

## References

<!-- txdoc:FAT-VFAT-REFERENCES-1 -->

- Microsoft EFI FAT32 Specification (fatgen103.doc) — 权威 on-disk 格式参考。
- [`BDEV_FS.md`](BDEV_FS.md) — bdev-fs 层设计。
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) — VFS checks / `FsOps` trait。
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — `FsPageBacking` trait。
- [`TX_EXT4_PLAN_v1_2.md`](TX_EXT4_PLAN_v1_2.md) — 架构参考。
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — factoring/topology。
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — `StepOutcome` 契约。
