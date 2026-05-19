//! Lightweight diagnostic probe for the VFS path-resolution state machine.
//!
//! The walker lives in a platform-independent crate (`tx-subsystems`) and
//! cannot emit console bytes directly.  This module provides two layers:
//!
//! 1. A single `AtomicU8` stage code for fast classification.
//! 2. A `DiagCtx` ring — structured context (dentry name, fs_object_id,
//!    path buffer) captured at the failure point so kernel-side sentinels
//!    can reconstruct *which* dentry lost its FsOps.
//!
//! Nothing here depends on HAL; everything is `core` + `tx-substrate`.

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::vfs::adapter::step_engine::SpinMutex;
use crate::vfs::structure::FsObjectId;

// === stage latch =====================================================

static DIAG_STAGE: AtomicU8 = AtomicU8::new(0);

pub fn record_diag(code: u8) {
    DIAG_STAGE.store(code, Ordering::Release);
}

pub fn last_diag() -> u8 {
    DIAG_STAGE.load(Ordering::Acquire)
}

// === structured context ===============================================

/// Rich context captured at a walker failure point.
#[derive(Clone, Debug)]
pub struct DiagCtx {
    /// Stage code (same as the AtomicU8 latch).
    pub stage: u8,
    /// Name of the dentry whose RNode failed to resolve FsOps.
    /// NUL-terminated, max 63 bytes.
    pub dentry_name: [u8; 64],
    /// FsObjectId of the RNode at the failure point.
    pub fs_object_id: u64,
    /// Snapshot of the remaining path at failure (NUL-terminated, max 127 bytes).
    pub remaining_path: [u8; 128],
    /// `containing_mount` was present on the RNode? (false = never stamped)
    pub had_containing_mount: bool,
}

static DIAG_CTX: SpinMutex<DiagCtx> = SpinMutex::new(DiagCtx {
    stage: 0,
    dentry_name: [0u8; 64],
    fs_object_id: 0,
    remaining_path: [0u8; 128],
    had_containing_mount: false,
});

/// Record a structured diagnostic context.  `dentry_name` and
/// `remaining` are truncated to fit.
pub fn record_ctx(
    stage: u8,
    dentry_name: &[u8],
    fs_object_id: FsObjectId,
    remaining: &[u8],
    had_containing_mount: bool,
) {
    let mut ctx = DIAG_CTX.lock();
    ctx.stage = stage;
    let nlen = dentry_name.len().min(63);
    ctx.dentry_name[..nlen].copy_from_slice(&dentry_name[..nlen]);
    ctx.dentry_name[nlen] = 0;
    ctx.fs_object_id = fs_object_id.as_u64();
    let rlen = remaining.len().min(127);
    ctx.remaining_path[..rlen].copy_from_slice(&remaining[..rlen]);
    ctx.remaining_path[rlen] = 0;
    ctx.had_containing_mount = had_containing_mount;
    DIAG_STAGE.store(stage, Ordering::Release);
}

/// Snapshot the structured context.  Caller must be in a spinlock-safe
/// context (boot path or interrupts off).
pub fn last_ctx() -> DiagCtx {
    DIAG_CTX.lock().clone()
}

// === compact label (legacy) ===========================================

static DIAG_LABEL: SpinMutex<[u8; 64]> = SpinMutex::new([0u8; 64]);

pub fn record_label(label: &[u8]) {
    let mut buf = DIAG_LABEL.lock();
    let len = label.len().min(63);
    buf[..len].copy_from_slice(&label[..len]);
    buf[len] = 0;
}

pub fn last_label() -> [u8; 64] {
    *DIAG_LABEL.lock()
}

// === render helpers ===================================================

/// Render the last `DiagCtx` into `buf` as a compact hex+ascii line.
/// Returns the number of bytes written (excluding trailing NUL).
///
/// Format: `stage=XX name=<ascii> fsid=<hex> path=<ascii> mount=<0|1>`
pub fn render_ctx(ctx: &DiagCtx, buf: &mut [u8; 256]) -> usize {
    let mut w = FixedWriter { buf, pos: 0 };

    // stage=XX
    let _ = write!(w, "stage={:02x} ", ctx.stage);

    // name=<ascii>
    let _ = write!(w, "name=");
    for &b in ctx.dentry_name.iter() {
        if b == 0 {
            break;
        }
        let _ = w.write_byte(b);
    }

    // fsid=<hex>
    let _ = write!(w, " fsid={:016x} ", ctx.fs_object_id);

    // path=<ascii>
    let _ = write!(w, "path=");
    for &b in ctx.remaining_path.iter() {
        if b == 0 {
            break;
        }
        let _ = w.write_byte(b);
    }

    // mount=<0|1>
    let _ = write!(w, " mount={}", ctx.had_containing_mount as u8);

    w.pos
}

struct FixedWriter<'a> {
    buf: &'a mut [u8; 256],
    pos: usize,
}

impl<'a> FixedWriter<'a> {
    fn write_byte(&mut self, b: u8) {
        if self.pos < self.buf.len() {
            self.buf[self.pos] = b;
            self.pos += 1;
        }
    }
}

impl core::fmt::Write for FixedWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.pos >= self.buf.len() {
                break;
            }
            self.buf[self.pos] = b;
            self.pos += 1;
        }
        Ok(())
    }
}
