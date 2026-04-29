# Phase A Plan (L3 类型与错误模型)

## 0. 计划定位

- Phase: A
- 分支: `feature-network`
- 范围: 仅网络模块内的 L3 基础类型、错误模型、StepOutcome、Wait 抽象与 stub runtime
- 非目标: 不实现 `StreamSocket/DatagramSocket` 状态机，不接 reactor，不接 VFS/fd/signal/driver

## 1. 规范依据（固定复核）

1. `msp/tx-kernel-network-stack-design-v9.md`
2. `docs/design/INDEX.md`
3. `docs/design/00_meta-framework/CONCEPTS_v4.md`
4. `docs/design/00_meta-framework/INVARIANTS_v4.md`
5. `docs/design/00_meta-framework/object_model_v2.md`
6. `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
7. `docs/design/02_execution/STEP_MODEL_v1.md`
8. `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md`

## 2. 本 phase 改动文件清单

1. `crates/tx-subsystems/src/lib.rs`
2. `crates/tx-subsystems/src/net/mod.rs`
3. `crates/tx-subsystems/src/net/core/mod.rs`
4. `crates/tx-subsystems/src/net/core/error.rs`
5. `crates/tx-subsystems/src/net/core/types.rs`
6. `crates/tx-subsystems/src/net/core/outcome.rs`
7. `crates/tx-subsystems/src/net/core/wait.rs`
8. `crates/tx-subsystems/src/net/core/readiness.rs`
9. `msp/network-l3-workflow/refactor-register.md`（登记本 phase 新增 `RFX-*`）
10. `msp/network-l3-workflow/phase_plan_result/phase-a-result.md`（实施后填写，不在本步骤创建内容）

目录对齐说明（与架构文档 §22）：

- 当前实现目录使用 `net/core/*`，用于承载“可复用基础语义层”。
- 语义上对应架构文档中的 `structure/*` + `protocol/*` 的基础类型部分，以及 `execution/*` 会消费的 outcome/wait/readiness 基础定义。
- 本 phase 只建基础层，不改变架构文档中的总体目录分工结论。

## 3. 文件级实现明细（真实代码结构）

### 3.1 `crates/tx-subsystems/src/lib.rs`

新增导出：

```rust
pub mod net;
```

### 3.2 `crates/tx-subsystems/src/net/mod.rs`

实现：

```rust
pub mod core;
```

### 3.3 `crates/tx-subsystems/src/net/core/mod.rs`

实现：

```rust
pub mod error;
pub mod types;
pub mod outcome;
pub mod wait;
pub mod readiness;

pub use error::{NetError, NetResult};
pub use outcome::StepOutcome;
pub use readiness::{AcceptWireSet, RecvWireSet, SendWireSet, SocketReadiness, UrgentEvent};
pub use types::{
    AddressFamily, IpEndpoint, KernelSockAddr, SendRecvFlags, SockShutdownCmd, SocketId,
    SocketKind,
};
pub use wait::{StubWaitRuntime, StubWaitRuntimeError, WaitKey, WaitRuntime, WaitToken};
```

### 3.4 `crates/tx-subsystems/src/net/core/error.rs`

实现：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetError {
    InvalidInput,
    AddressFamilyNotSupported,
    SocketTypeNotSupported,
    ProtocolNotSupported,
    NotConnected,
    AlreadyConnected,
    WouldBlock,
    ConnectionRefused,
    ConnectionReset,
    TimedOut,
    BrokenPipe,
    NoBufferSpace,
    NotSupported,
    Internal,
}

pub type NetResult<T> = core::result::Result<T, NetError>;
```

### 3.5 `crates/tx-subsystems/src/net/core/types.rs`

实现：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SocketId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressFamily {
    Inet,
    Inet6,
    Unix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    Stream,
    Datagram,
    Raw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendRecvFlags(pub u32);

impl SendRecvFlags {
    pub const NONE: Self = Self(0);
    pub const DONTWAIT: Self = Self(1 << 0);
    pub const PEEK: Self = Self(1 << 1);
    pub const OOB: Self = Self(1 << 2);

    pub const fn contains(self, rhs: Self) -> bool {
        (self.0 & rhs.0) == rhs.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SockShutdownCmd {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Ipv4Addr {
    pub octets: [u8; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Ipv6Addr {
    pub octets: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IpEndpoint {
    V4 { addr: Ipv4Addr, port: u16 },
    V6 { addr: Ipv6Addr, port: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum KernelSockAddr {
    Inet(IpEndpoint),
}

impl KernelSockAddr {
    pub const fn family(&self) -> AddressFamily {
        match self {
            KernelSockAddr::Inet(IpEndpoint::V4 { .. }) => AddressFamily::Inet,
            KernelSockAddr::Inet(IpEndpoint::V6 { .. }) => AddressFamily::Inet6,
        }
    }
}
```

### 3.6 `crates/tx-subsystems/src/net/core/outcome.rs`

实现：

```rust
use crate::net::core::error::NetError;
use crate::net::core::wait::WaitKey;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome<T> {
    Done(T),
    Blocked(WaitKey),
    Advanced(T),
    AdvancedThenBlocked { progress: T, wait: WaitKey },
    Err(NetError),
}
```

### 3.7 `crates/tx-subsystems/src/net/core/wait.rs`

实现（含 stub runtime，供后续测试手动触发 ready）。以下为必须落地的公开 API 签名：

```rust
use crate::net::core::types::SocketId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WaitKey {
    RecvReadable(SocketId),
    SendWritable(SocketId),
    AcceptPending(SocketId),
    ConnectProgress(SocketId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WaitToken {
    pub key: WaitKey,
}

pub trait WaitRuntime {
    fn subscribe(&mut self, key: WaitKey) -> WaitToken;
    fn is_ready(&self, token: WaitToken) -> bool;
    fn clear_ready(&mut self, key: WaitKey);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StubWaitRuntimeError {
    CapacityFull,
}

pub struct StubWaitRuntime<const N: usize> {
    ready: [Option<WaitKey>; N],
}

impl<const N: usize> StubWaitRuntime<N> {
    pub const fn new() -> Self {
        Self { ready: [None; N] }
    }

    // REFACTOR(net-l3): [RFX-002] 接入 reactor 后替换 StubWaitRuntime。
    // Trigger: reactor wait token ready
    // Keep-until: L1/L2 接线完成
    pub fn mark_ready(&mut self, key: WaitKey) -> Result<(), StubWaitRuntimeError>;
    pub fn clear_all(&mut self);
}

impl<const N: usize> WaitRuntime for StubWaitRuntime<N> {
    fn subscribe(&mut self, key: WaitKey) -> WaitToken;
    fn is_ready(&self, token: WaitToken) -> bool;
    fn clear_ready(&mut self, key: WaitKey);
}
```

### 3.8 `crates/tx-subsystems/src/net/core/readiness.rs`

实现（仅语义类型，不接 substrate bus carrier）。字段命名与架构文档保持一致：`recv_wq/send_wq/accept_wq/urgent_port`。
以下为必须落地的公开 API 签名：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvWireSet {
    pub has_data: bool,
    pub broken: bool,
}

impl RecvWireSet {
    pub const fn empty() -> Self;
    pub const fn has_data() -> Self;
    pub const fn broken() -> Self;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendWireSet {
    pub space: bool,
    pub broken: bool,
}

impl SendWireSet {
    pub const fn empty() -> Self;
    pub const fn space() -> Self;
    pub const fn broken() -> Self;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptWireSet {
    pub has_pending: bool,
    pub broken: bool,
}

impl AcceptWireSet {
    pub const fn empty() -> Self;
    pub const fn has_pending() -> Self;
    pub const fn broken() -> Self;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrgentEvent {
    Urgent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketReadiness {
    pub recv_wq: RecvWireSet,
    pub send_wq: SendWireSet,
    pub accept_wq: AcceptWireSet,
    pub urgent_port: Option<UrgentEvent>,
}

impl SocketReadiness {
    pub const fn new() -> Self;
}
```

## 4. 预登记 RFX（本 phase）

1. `RFX-002`: `StubWaitRuntime` 未来替换为 reactor runtime adapter。
2. `RFX-003`: readiness 语义类型未来映射到 substrate `RawQueue/RawPort`。
3. `RFX-004`: `NetError` 未来映射到内核统一错误码层。

## 5. 验收标准

1. `cargo check` 通过。
2. Phase A 所有文件与导出可编译。
3. 无跨模块硬依赖调用（仅本模块类型定义）。
4. 所有临时点都有 `REFACTOR(net-l3)` + `RFX-*`，且登记到 `refactor-register.md`。

## 6. 实施顺序

1. 建模块与导出（`lib.rs`、`net/mod.rs`、`net/core/mod.rs`）。
2. 实现 `error.rs`、`types.rs`。
3. 实现 `outcome.rs`、`wait.rs`。
4. 实现 `readiness.rs`。
5. 更新 `refactor-register.md`。
6. 运行 `cargo check`。
