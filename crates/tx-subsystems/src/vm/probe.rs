//! PROBE(proxy-push segv hunt): raw console sink + watched-VA event log.
//!
//! A mallocng meta page (deterministically at 0xafb000 in the repro) is
//! observed all-zero at crash time. These probes narrate every VM event that
//! touches the watched page — publishes, CoW replacements, zero-fills,
//! teardowns, fork sharing — so the page's life story pins the corrupting
//! transition. Remove once the bug is fixed.

use core::sync::atomic::{AtomicUsize, Ordering};

/// User VA range under watch (page containing the corrupted mallocng meta).
pub const WATCH_LO: usize = 0xafb000;
pub const WATCH_HI: usize = 0xafc000;

/// Returns true when `[start, end)` overlaps the watch window.
pub(crate) fn watch_overlap(start: usize, end: usize) -> bool {
    start < WATCH_HI && end > WATCH_LO
}

type Sink = fn(&[u8]);
const NO_SINK: usize = 0;
static SINK: AtomicUsize = AtomicUsize::new(NO_SINK);

/// Install the raw console sink (called once from kernel init).
pub fn install_probe_sink(sink: Sink) {
    SINK.store(sink as usize, Ordering::Release);
}

fn push_hex(buf: &mut [u8], n: &mut usize, value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 16];
    let mut len = 0;
    let mut v = value;
    if v == 0 {
        digits[0] = b'0';
        len = 1;
    } else {
        while v != 0 {
            digits[len] = HEX[(v & 0xf) as usize];
            v >>= 4;
            len += 1;
        }
    }
    for k in (0..len).rev() {
        if *n < buf.len() {
            buf[*n] = digits[k];
            *n += 1;
        }
    }
}

/// Emit one watch event line: `txkernel:vmwatch:<tag> <hex> <hex> ...`.
pub fn probe_emit(tag: &str, vals: &[u64]) {
    let raw = SINK.load(Ordering::Acquire);
    if raw == NO_SINK {
        return;
    }
    let sink: Sink = unsafe { core::mem::transmute(raw) };
    let mut buf = [0u8; 224];
    let mut n = 0;
    for b in b"txkernel:vmwatch:" {
        buf[n] = *b;
        n += 1;
    }
    for b in tag.bytes() {
        if n < buf.len() {
            buf[n] = b;
            n += 1;
        }
    }
    for v in vals {
        if n < buf.len() {
            buf[n] = b' ';
            n += 1;
        }
        push_hex(&mut buf, &mut n, *v);
    }
    if n < buf.len() {
        buf[n] = b'\n';
        n += 1;
    }
    sink(&buf[..n]);
}

/// First 8 bytes of the frame at `ppn` (via the direct map), as a content
/// fingerprint. Returns u64::MAX when the direct-map lookup is unavailable.
pub(crate) fn frame_fingerprint(ppn: tx_hal::Ppn) -> u64 {
    match crate::vm::adapter::step_engine::page_allocator::frame_kernel_addr(ppn) {
        Ok(ptr) => unsafe { core::ptr::read_volatile(ptr as *const u64) },
        Err(_) => u64::MAX,
    }
}
