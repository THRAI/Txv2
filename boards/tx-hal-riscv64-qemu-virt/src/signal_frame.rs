//! RV64 userspace signal-frame ABI helpers.
//!
//! This module only owns the board/architecture mechanics: frame layout on the
//! selected user stack, trap-frame register rewrites for handler entry, and
//! context restore for sigreturn. Signal routing and POSIX policy live above
//! HAL.

use core::mem::{offset_of, size_of};

use crate::user_access::{board_copy_from_user, board_copy_to_user};
use crate::Platform;
use tx_hal::{
    FaultInfo, Pod, SavedSignalFrame, SignalFrameIf, SignalFramePlacement, SignalFrameWrite,
    SignalHandlerRegs, TrapFrameMut, UserPtr, UserSignalMaskAbi, UserTrapContext, VirtAddr,
};

const RV64_SIGFRAME_ALIGN: usize = 16;
const RV64_SIGFRAME_MAGIC: u64 = 0x5458_5632_5349_4731; // "TXV2SIG1"
const RV64_SIGFRAME_VERSION: u32 = 1;
const RV64_RT_SIGRETURN_SYSCALL: u32 = 139;
const RV64_ECALL: u32 = 0x0000_0073;

const fn rv64_addi(rd: u32, rs1: u32, imm: u32) -> u32 {
    ((imm & 0x0fff) << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

const RV64_SIGRETURN_TRAMPOLINE: [u32; 2] = [
    rv64_addi(17, 0, RV64_RT_SIGRETURN_SYSCALL), // addi a7, zero, __NR_rt_sigreturn
    RV64_ECALL,
];

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Rv64SignalFrame {
    magic: u64,
    version: u32,
    frame_size: u32,
    sig_no: u32,
    _reserved0: u32,
    flags: u64,
    siginfo: tx_hal::UserSigInfoAbi,
    saved_mask: UserSignalMaskAbi,
    user_context: UserTrapContext,
    trampoline: [u32; 2],
}

unsafe impl Pod for Rv64SignalFrame {}

impl Rv64SignalFrame {
    fn new(tf: &TrapFrameMut<'_>, setup: &SignalFrameWrite) -> Self {
        Self {
            magic: RV64_SIGFRAME_MAGIC,
            version: RV64_SIGFRAME_VERSION,
            frame_size: size_of::<Self>() as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            siginfo: setup.siginfo,
            saved_mask: setup.old_mask,
            user_context: tf.capture_user_context(),
            trampoline: RV64_SIGRETURN_TRAMPOLINE,
        }
    }

    fn validate(&self, user_sp: UserPtr<u8>) -> Result<(), FaultInfo> {
        if self.magic == RV64_SIGFRAME_MAGIC
            && self.version == RV64_SIGFRAME_VERSION
            && self.frame_size as usize == size_of::<Self>()
            && self.trampoline == RV64_SIGRETURN_TRAMPOLINE
        {
            Ok(())
        } else {
            Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: false,
            })
        }
    }
}

impl SignalFrameIf for Platform {
    fn write_signal_frame(
        mut tf: TrapFrameMut<'_>,
        setup: SignalFrameWrite,
    ) -> Result<SignalFramePlacement, FaultInfo> {
        let frame_size = size_of::<Rv64SignalFrame>();
        let Some(unrounded_frame_addr) = setup.stack_top.addr().checked_sub(frame_size) else {
            return Err(FaultInfo {
                address: VirtAddr(setup.stack_top.addr()),
                write: true,
                instruction: false,
                from_user: false,
            });
        };
        let frame_addr = align_down(unrounded_frame_addr, RV64_SIGFRAME_ALIGN);

        let frame = Rv64SignalFrame::new(&tf, &setup);
        let user_frame = UserPtr::<Rv64SignalFrame>::new(frame_addr);

        // SAFETY: the stack address was selected by signal delivery policy as
        // a user stack. `board_copy_to_user` performs the actual user copy
        // through the SUM/fixup-table primitive and converts faults into
        // FaultInfo.
        unsafe {
            let bytes = core::slice::from_raw_parts(
                core::ptr::addr_of!(frame).cast::<u8>(),
                size_of::<Rv64SignalFrame>(),
            );
            board_copy_to_user(UserPtr::<u8>::new(user_frame.addr()), bytes)?;
        }

        let siginfo_addr = frame_addr + offset_of!(Rv64SignalFrame, siginfo);
        let ucontext_addr = frame_addr + offset_of!(Rv64SignalFrame, user_context);
        let trampoline_pc = frame_addr + offset_of!(Rv64SignalFrame, trampoline);

        tf.set_pc(VirtAddr(setup.handler_pc.addr()));
        tf.set_sp(VirtAddr(frame_addr));
        tf.set_signal_handler_regs(SignalHandlerRegs {
            return_pc: VirtAddr(trampoline_pc),
            args: [setup.sig_no as usize, siginfo_addr, ucontext_addr],
        });

        Ok(SignalFramePlacement {
            frame_addr: UserPtr::new(frame_addr),
            trampoline_pc: UserPtr::new(trampoline_pc),
        })
    }

    fn read_signal_frame(user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo> {
        // SAFETY: sigreturn supplies the current user SP.
        // `board_copy_from_user` performs the checked copy through the
        // SUM/fixup-table primitive and reports any bad frame pointer.
        let mut frame = core::mem::MaybeUninit::<Rv64SignalFrame>::uninit();
        let frame = unsafe {
            let dst = core::slice::from_raw_parts_mut(
                frame.as_mut_ptr().cast::<u8>(),
                size_of::<Rv64SignalFrame>(),
            );
            board_copy_from_user(dst, UserPtr::<u8>::new(user_sp.addr()))?;
            frame.assume_init()
        };

        frame.validate(user_sp)?;
        Ok(SavedSignalFrame {
            saved_mask: frame.saved_mask,
            user_context: frame.user_context,
        })
    }

    fn restore_signal_frame(mut tf: TrapFrameMut<'_>, frame: &SavedSignalFrame) {
        tf.restore_user_context(&frame.user_context);
    }

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }
}

fn align_down(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rv64_trampoline_encodes_rt_sigreturn_ecall() {
        assert_eq!(RV64_SIGRETURN_TRAMPOLINE, [0x08b0_0893, RV64_ECALL]);
    }

    #[test]
    fn rv64_signal_frame_is_stack_aligned() {
        assert_eq!(size_of::<Rv64SignalFrame>() % RV64_SIGFRAME_ALIGN, 0);
        assert_eq!(
            offset_of!(Rv64SignalFrame, trampoline) % core::mem::align_of::<u32>(),
            0
        );
    }

    #[test]
    fn align_down_rounds_to_requested_boundary() {
        assert_eq!(align_down(0x100f, 16), 0x1000);
        assert_eq!(align_down(0x1000, 16), 0x1000);
    }
}
