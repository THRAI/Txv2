//! User-space signal ABI: siginfo / mask / sa-flags records, the
//! `SignalFrameWrite` setup descriptor, `SignalFrameBytes` / `SavedSignalFrame`,
//! and the `SignalFrameIf` platform hook that boards implement to build and
//! restore signal frames.

use crate::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSigInfoAbi {
    pub bytes: [u8; 128],
}

impl UserSigInfoAbi {
    pub const ZERO: Self = Self { bytes: [0; 128] };
}

unsafe impl Pod for UserSigInfoAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSignalMaskAbi {
    pub bits: u64,
}

impl UserSignalMaskAbi {
    pub const EMPTY: Self = Self { bits: 0 };
}

unsafe impl Pod for UserSignalMaskAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSaFlagsAbi {
    pub bits: u64,
}

impl UserSaFlagsAbi {
    pub const EMPTY: Self = Self { bits: 0 };
}

unsafe impl Pod for UserSaFlagsAbi {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalFrameWrite {
    pub stack_top: UserPtr<u8>,
    pub sig_no: u32,
    pub siginfo: UserSigInfoAbi,
    pub old_mask: UserSignalMaskAbi,
    pub flags: UserSaFlagsAbi,
    pub handler_pc: UserPtr<()>,
    pub restorer_pc: UserPtr<()>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalFramePlacement {
    pub frame_addr: UserPtr<()>,
    pub trampoline_pc: UserPtr<()>,
}

/// Raw bytes of a signal frame (platform-specific layout).
/// Carried from `prepare_signal_frame` to the caller, who writes
/// them to the user stack via `AddressSpace::copy_to_user`.
///
/// The buffer must be at least as large as the platform-specific
/// `*SignalFrame` struct (RV64 ~720 bytes including UserTrapContext +
/// FpContext + trampoline). The previous 512-byte buffer silently
/// truncated `from_slice`, dropping the trailing fields — most
/// catastrophically the on-stack `rt_sigreturn` trampoline at
/// `offset_of!(SignalFrame, trampoline) = 712` — so the handler
/// returned through `ra = frame_addr + 712` and the CPU fetched
/// uninitialised stack bytes instead of the trampoline. Musl-compatible
/// RV64 `ucontext_t` now includes the full floating-point union, so
/// keep the carrier above the board frame sizes rather than trimming
/// the userspace ABI shape.
pub struct SignalFrameBytes {
    pub data: [u8; Self::CAPACITY],
    pub len: usize,
}

impl SignalFrameBytes {
    pub const CAPACITY: usize = 2048;

    pub fn from_slice(bytes: &[u8]) -> Self {
        let len = bytes.len();
        assert!(
            len <= Self::CAPACITY,
            "signal frame layout ({len} bytes) exceeds SignalFrameBytes buffer ({})",
            Self::CAPACITY,
        );
        let mut data = [0u8; Self::CAPACITY];
        data[..len].copy_from_slice(bytes);
        Self { data, len }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SavedSignalFrame {
    pub saved_mask: UserSignalMaskAbi,
    pub user_context: UserTrapContext,
}

unsafe impl Pod for SavedSignalFrame {}

pub trait SignalFrameIf: TrapIf {

    fn signal_frame_size() -> usize {
        0
    }

    fn decode_signal_frame_bytes(
        user_sp: UserPtr<u8>,
        _bytes: &[u8],
    ) -> Result<SavedSignalFrame, FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(user_sp.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }

    fn restore_signal_frame(_tf: TrapFrameMut<'_>, _frame: &SavedSignalFrame) {}

    /// Build a signal-handler entry context and frame bytes
    /// WITHOUT accessing a live TrapFrameMut. Returns the modified
    /// `UserTrapContext` (sepc=handler, sp=frame_addr, ra=trampoline)
    /// and the raw signal frame bytes to write to the user stack.
    ///
    /// Used by the thread-future AST checkpoint, which runs before
    /// `enter_userspace_with_context` (where TrapFrameMut is
    /// available). The caller writes `frame_bytes` to user memory
    /// via `AddressSpace::copy_to_user`, then stores the modified
    /// context as `saved_user_context`.
    ///
    /// Default: returns `ENOSYS`-shaped fallback.
    fn prepare_signal_frame(
        _ctx: &UserTrapContext,
        _setup: &SignalFrameWrite,
    ) -> Result<(UserTrapContext, SignalFrameBytes), FaultInfo> {
        Err(FaultInfo {
            address: VirtAddr(0),
            write: true,
            instruction: false,
            from_user: false,
        })
    }
}
