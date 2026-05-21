//! Termios structure and flag constants.
//!
//! Flag values and `c_cc` indices match the Linux ABI (`asm-generic/termbits.h`)
//! for architecture compatibility.

// ---------------------------------------------------------------------------
// Sizing constants
// ---------------------------------------------------------------------------

/// Number of kernel control-character slots.
///
/// Linux generic `TCGETS` / `TCSETS` copies the kernel `struct termios`
/// shape, whose control-character array is 19 bytes. musl's public
/// `struct termios` is larger (`NCCS = 32` plus speed fields), but its
/// `tcgetattr` / `tcsetattr` wrappers pass that buffer directly to these
/// ioctls; the kernel-populated prefix is the ABI contract.
pub const NCCS: usize = 19;

/// Value indicating a disabled control character (`_POSIX_VDISABLE`).
pub const POSIX_VDISABLE: u8 = 0xff;

/// Maximum bytes in the canonical (cooked) input buffer.
pub const MAX_CANON: usize = 255;

// ---------------------------------------------------------------------------
// c_cc control-character indices
// ---------------------------------------------------------------------------

pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VEOL: usize = 11;
pub const VREPRINT: usize = 12;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;

// ---------------------------------------------------------------------------
// c_iflag bits
// ---------------------------------------------------------------------------

/// Map NL to CR on input.
pub const INLCR: u32 = 0x0040;
/// Ignore CR.
pub const IGNCR: u32 = 0x0080;
/// Map CR to NL on input.
pub const ICRNL: u32 = 0x0100;
/// Enable XON/XOFF flow control on output.
pub const IXON: u32 = 0x0400;
/// Any character restarts stopped output (requires IXON).
pub const IXANY: u32 = 0x0800;
/// Enable XON/XOFF flow control on input.
pub const IXOFF: u32 = 0x1000;

// ---------------------------------------------------------------------------
// c_oflag bits
// ---------------------------------------------------------------------------

/// Enable output post-processing.
pub const OPOST: u32 = 0x01;
/// Map NL to CR-NL on output.
pub const ONLCR: u32 = 0x04;
/// Map CR to NL on output.
pub const OCRNL: u32 = 0x08;
/// No CR output at column 0.
pub const ONOCR: u32 = 0x10;
/// NL performs CR function (resets column counter).
pub const ONLRET: u32 = 0x20;

// ---------------------------------------------------------------------------
// c_lflag bits
// ---------------------------------------------------------------------------

/// Generate signals on INTR, QUIT, SUSP.
pub const ISIG: u32 = 0x0001;
/// Enable canonical (cooked) mode.
pub const ICANON: u32 = 0x0002;
/// Enable echo.
pub const ECHO: u32 = 0x0008;
/// Echo erase character as BS-SP-BS.
pub const ECHOE: u32 = 0x0010;
/// Echo KILL by erasing the line.
pub const ECHOK: u32 = 0x0020;
/// Echo NL even if ECHO is off.
pub const ECHONL: u32 = 0x0040;
/// Enable extended processing (VLNEXT, VWERASE, etc.).
pub const IEXTEN: u32 = 0x8000;
/// Background writes generate SIGTTOU.
pub const TOSTOP: u32 = 0x0100;

// ---------------------------------------------------------------------------
// Termios struct
// ---------------------------------------------------------------------------

/// Terminal I/O settings.
///
/// Layout matches `struct termios` from the Linux ABI. `c_cflag` (baud rate,
/// character size, modem control) is carried for ABI compatibility but is not
/// interpreted by the line discipline; hardware drivers consume it separately.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
}

const _: () = assert!(core::mem::size_of::<Termios>() == 36);

impl Termios {
    /// All-zero termios (raw, no processing, no signals).
    pub const fn zeroed() -> Self {
        Self {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0u8; NCCS],
        }
    }

    /// Standard cooked-mode settings, matching the Linux shell default.
    ///
    /// Enables: ICRNL, IXON, IXANY, OPOST, ONLCR, ISIG, ICANON, ECHO,
    /// ECHOE, ECHOK, ECHONL, IEXTEN. Control characters follow stty(1) defaults.
    pub fn default_cooked() -> Self {
        let mut t = Self::zeroed();
        t.c_iflag = ICRNL | IXON | IXANY;
        t.c_oflag = OPOST | ONLCR;
        t.c_lflag = ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHONL | IEXTEN;

        let cc = &mut t.c_cc;
        cc[VINTR] = 0x03; // ^C
        cc[VQUIT] = 0x1c; // ^\
        cc[VERASE] = 0x7f; // DEL
        cc[VKILL] = 0x15; // ^U
        cc[VEOF] = 0x04; // ^D
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSTART] = 0x11; // ^Q
        cc[VSTOP] = 0x13; // ^S
        cc[VSUSP] = 0x1a; // ^Z
        cc[VEOL] = POSIX_VDISABLE;
        cc[VREPRINT] = 0x12; // ^R
        cc[VWERASE] = 0x17; // ^W
        cc[VLNEXT] = 0x16; // ^V
        t
    }

    /// Pty-master termios: all flags cleared, effectively raw mode.
    ///
    /// The pty master is raw because the application (ssh, xterm) wants
    /// unprocessed bytes. Behavioral differences are expressed via termios
    /// flags, not a separate line discipline (LDSC-1 from TTY.md §3).
    pub fn default_pty_master() -> Self {
        let mut t = Self::zeroed();
        // Keep c_cc defaults for predictability even though the disabled flags
        // mean none of them will fire.
        let cc = &mut t.c_cc;
        cc[VINTR] = 0x03;
        cc[VQUIT] = 0x1c;
        cc[VERASE] = 0x7f;
        cc[VKILL] = 0x15;
        cc[VEOF] = 0x04;
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSTART] = 0x11;
        cc[VSTOP] = 0x13;
        cc[VSUSP] = 0x1a;
        cc[VEOL] = POSIX_VDISABLE;
        cc[VREPRINT] = 0x12;
        cc[VWERASE] = 0x17;
        cc[VLNEXT] = 0x16;
        t
    }
}
