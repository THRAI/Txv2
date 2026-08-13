//! 通用 pmap 页范围助手（契约层）。
//!
//! 这里维护的核心数据结构/状态：
//! - `PmapRangeReservation<P, N>`：栈上、无堆分配的事务，最多持有 `N` 个已预约的
//!   `PmapReservation`。
//! - unmap/protect 证据由调用方提供输出切片；本模块不分配结果缓冲区。
//!
//! 主要的数据流函数：
//! - `reserve_page_range()`：预约一段连续的 4 KiB 范围并返回事务对象。
//! - `PmapRangeReservation::commit()`：提交（发布）全部已预约的页。
//! - `PmapRangeReservation` 的 `Drop`：回滚任何尚未提交的前缀。
//! - `unmap_page_range()` 与 `protect_page_range()`：逐页收集证据，供后续
//!   shootdown/计数使用。
//!
//! 辅助逻辑仅限于带溢出检查的翻页步进、输出初始化和错误归一化。具体的页表遍历与
//! PTE 更新仍归板卡所有；范围锁和重物化策略仍归 VM 所有。参见
//! `docs/progress/decisions/2026-04-29-hal-pmap-surface-refactor.md`。

use core::marker::PhantomData;
use core::mem::MaybeUninit;

use super::{
    PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, VirtAddr,
};

/// 4 KiB 页大小。
const PAGE_SIZE_4K: usize = 4096;

/// 页范围操作的错误类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PmapRangeError {
    EmptyRange,         // 页数为 0。
    BufferTooSmall,     // 页数超过 N 或输出切片容量。
    AddressOverflow,    // 虚/物地址步进时溢出。
    Pmap(PmapError),    // 底层 pmap 返回的错误。
    MissingReservation, // reserve_mapping 返回 None（预期有预约却没有）。
}

/// 固定容量的页表失效区间收集器。
///
/// 相邻区间会直接合并，容量耗尽时保留精确的重试游标，供 substrate
/// 分批完成 shootdown，整个过程不需要堆分配。
pub struct InvalidationRunGather<const N: usize> {
    entries: [MaybeUninit<PmapInvalidation>; N],
    len: usize,
    next_cursor: VirtAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidationRunGatherError {
    kind: InvalidationRunGatherErrorKind,
    next_cursor: VirtAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidationRunGatherErrorKind {
    Capacity,
    InvalidCursor,
    AddressOverflow,
}

impl InvalidationRunGatherError {
    pub const fn kind(self) -> InvalidationRunGatherErrorKind {
        self.kind
    }

    pub const fn next_cursor(self) -> VirtAddr {
        self.next_cursor
    }
}

impl<const N: usize> InvalidationRunGather<N> {
    pub const fn new(next_cursor: VirtAddr) -> Self {
        Self {
            entries: [const { MaybeUninit::uninit() }; N],
            len: 0,
            next_cursor,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn next_cursor(&self) -> VirtAddr {
        self.next_cursor
    }

    pub fn push(
        &mut self,
        invalidation: PmapInvalidation,
        next_cursor: VirtAddr,
    ) -> Result<(), InvalidationRunGatherError> {
        let Some(invalidation_end) = invalidation.virt().0.checked_add(invalidation.size()) else {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::AddressOverflow,
                next_cursor: self.next_cursor,
            });
        };
        if next_cursor.0 <= self.next_cursor.0 || next_cursor.0 < invalidation_end {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::InvalidCursor,
                next_cursor: self.next_cursor,
            });
        }
        if let Some(last) = self.last_mut() {
            let Some(last_end) = last.virt().0.checked_add(last.size()) else {
                return Err(InvalidationRunGatherError {
                    kind: InvalidationRunGatherErrorKind::AddressOverflow,
                    next_cursor: self.next_cursor,
                });
            };
            if last_end == invalidation.virt().0 {
                let Some(size) = last.size().checked_add(invalidation.size()) else {
                    return Err(InvalidationRunGatherError {
                        kind: InvalidationRunGatherErrorKind::AddressOverflow,
                        next_cursor: self.next_cursor,
                    });
                };
                *last = PmapInvalidation::new(last.virt(), size);
                self.next_cursor = next_cursor;
                return Ok(());
            }
        }
        if self.len == N {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::Capacity,
                next_cursor: self.next_cursor,
            });
        }
        self.entries[self.len].write(invalidation);
        self.len += 1;
        self.next_cursor = next_cursor;
        Ok(())
    }

    pub fn as_slice(&self) -> &[PmapInvalidation] {
        // SAFETY: `push` 保证 [0, len) 已初始化，MaybeUninit<T> 与 T 布局相同。
        unsafe { core::slice::from_raw_parts(self.entries.as_ptr().cast(), self.len) }
    }

    pub fn clear(&mut self, next_cursor: VirtAddr) {
        self.len = 0;
        self.next_cursor = next_cursor;
    }

    fn last_mut(&mut self) -> Option<&mut PmapInvalidation> {
        if self.len == 0 {
            None
        } else {
            // SAFETY: `len` 以下的元素均由 `push` 初始化。
            Some(unsafe { self.entries[self.len - 1].assume_init_mut() })
        }
    }
}

impl From<PmapError> for PmapRangeError {
    fn from(value: PmapError) -> Self {
        Self::Pmap(value)
    }
}

/// 一段页范围的栈上预约事务。
///
/// 条目存放在 `MaybeUninit` 中，故调用方最多可预约 `N` 页而无需堆分配。丢弃一个
/// 尚未提交的值会通过平台 pmap 回滚已预约的前缀。
pub struct PmapRangeReservation<'a, P: PmapIf, const N: usize> {
    root: &'a PmapRoot,
    entries: [MaybeUninit<PmapReservation>; N],
    len: usize,
    _platform: PhantomData<P>,
    _not_send: PhantomData<*const ()>,
}

impl<'a, P: PmapIf, const N: usize> PmapRangeReservation<'a, P, N> {
    /// 新建一个空事务，绑定到给定页表根。
    fn new(root: &'a PmapRoot) -> Self {
        Self {
            root,
            entries: [const { MaybeUninit::uninit() }; N],
            len: 0,
            _platform: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// 当前已预约的页数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否尚无任何预约。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 追加一个预约到栈上数组并递增计数。
    fn push(&mut self, reservation: PmapReservation) {
        self.entries[self.len].write(reservation);
        self.len += 1;
    }

    /// 提交全部预约：先清零 len 让 Drop 不再回滚，再逐个正式建立映射。
    pub fn commit(mut self, permissions: PmapPermissions) -> usize {
        let len = self.len;
        self.len = 0; // 置 0：一旦开始提交，Drop 就不应再回滚这些条目。
        for index in 0..len {
            let reservation = unsafe { self.entries[index].assume_init_read() };
            P::commit_mapping(self.root, reservation, permissions);
        }
        len
    }
}

impl<P: PmapIf, const N: usize> Drop for PmapRangeReservation<'_, P, N> {
    /// RAII 回滚：未提交（commit 未消费）时，逐个撤销已预约的前缀。
    fn drop(&mut self) {
        for index in 0..self.len {
            let reservation = unsafe { self.entries[index].assume_init_read() };
            P::rollback_mapping(self.root, reservation);
        }
        self.len = 0;
    }
}

// 范围操作刻意是页大小的 v1 版助手。它们把 unmap/protect 结果收集进调用方提供的
// 切片，以便 substrate 或 VM 在 shootdown 后将失效与 map-count 释放配对。

/// 预约一段连续的 4 KiB 范围，返回可提交/可回滚的事务对象。
pub fn reserve_page_range<'a, P: PmapIf, const N: usize>(
    root: &'a PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    pages: usize,
) -> Result<PmapRangeReservation<'a, P, N>, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > N {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut range = PmapRangeReservation::<P, N>::new(root);
    for index in 0..pages {
        // 逐页步进：偏移与虚/物地址均带溢出检查。
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        let page_phys = PhysAddr(
            phys.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        // 单页预约；出错时 range 的 Drop 会自动回滚已攒下的前缀。
        let reservation = P::reserve_mapping(root, page_virt, page_phys, PmapReserveKind::Page4K)?
            .ok_or(PmapRangeError::MissingReservation)?;
        range.push(reservation);
    }
    Ok(range)
}

/// 解除一段页范围的映射，逐页把 unmap 结果写入调用方的 out 切片，返回条目数。
pub fn unmap_page_range<P: PmapIf>(
    root: &PmapRoot,
    virt: VirtAddr,
    pages: usize,
    out: &mut [Option<PmapUnmapResult>],
) -> Result<usize, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > out.len() {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut len = 0;
    for slot in out.iter_mut().take(pages) {
        *slot = None; // 先把输出槽清空。
    }
    for index in 0..pages {
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        // 只有产生失效证据的页才紧凑写入 out（未映射页跳过）。
        if let Some(result) = P::unmap_mapping(root, page_virt, PmapReserveKind::Page4K)? {
            out[len] = Some(result);
            len += 1;
        }
    }
    Ok(len)
}

/// 修改一段页范围的权限，逐页把失效证据写入 out 切片，返回条目数。
pub fn protect_page_range<P: PmapIf>(
    root: &PmapRoot,
    virt: VirtAddr,
    pages: usize,
    permissions: PmapPermissions,
    out: &mut [Option<PmapInvalidation>],
) -> Result<usize, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > out.len() {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut len = 0;
    for slot in out.iter_mut().take(pages) {
        *slot = None; // 先把输出槽清空。
    }
    for index in 0..pages {
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        // 只有真正改动权限产生失效的页才紧凑写入 out。
        if let Some(invalidation) =
            P::protect_mapping(root, page_virt, PmapReserveKind::Page4K, permissions)?
        {
            out[len] = Some(invalidation);
            len += 1;
        }
    }
    Ok(len)
}
