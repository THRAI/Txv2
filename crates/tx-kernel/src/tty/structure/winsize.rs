//! Terminal window size.

/// Terminal window size, matching `struct winsize` from `<sys/ioctl.h>`.
///
/// Packed into a `u64` for atomic storage via `TtyPayload::window_size`
/// (Phase B). Ordering: `ws_row` in the high 16 bits, `ws_ypixel` in the
/// low 16 bits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl Winsize {
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    /// Pack into a `u64` for atomic storage.
    ///
    /// Layout: `[ws_row(63:48) | ws_col(47:32) | ws_xpixel(31:16) | ws_ypixel(15:0)]`.
    pub const fn to_u64(self) -> u64 {
        ((self.ws_row as u64) << 48)
            | ((self.ws_col as u64) << 32)
            | ((self.ws_xpixel as u64) << 16)
            | (self.ws_ypixel as u64)
    }

    /// Unpack from a `u64`.
    pub const fn from_u64(val: u64) -> Self {
        Self {
            ws_row: ((val >> 48) & 0xffff) as u16,
            ws_col: ((val >> 32) & 0xffff) as u16,
            ws_xpixel: ((val >> 16) & 0xffff) as u16,
            ws_ypixel: (val & 0xffff) as u16,
        }
    }
}
