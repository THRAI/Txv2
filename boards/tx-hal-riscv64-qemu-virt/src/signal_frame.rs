//! RV64 userspace signal-frame ABI helpers.
//!
//! This module only owns the board/architecture mechanics: frame layout on the
//! selected user stack, trap-frame register rewrites for handler entry, and
//! context restore for sigreturn. Signal routing and POSIX policy live above
//! HAL.
//!
//! The signal frame contains a Linux-compatible `ucontext_t` so that musl libc
//! signal handlers (especially the pthread cancel handler) can read and modify
//! the interrupted register state via `uc_mcontext.__gregs[0]` = PC.

use core::mem::{offset_of, size_of};

use crate::user_access::{board_copy_from_user, board_copy_to_user};
use crate::Platform;
use tx_hal::{
    FaultInfo, Pod, SavedSignalFrame, SignalFrameBytes, SignalFrameIf, SignalFramePlacement,
    SignalFrameWrite, SignalHandlerRegs, TrapFrameMut, UserPtr, UserSignalMaskAbi, UserTrapContext,
    VirtAddr,
};

const RV64_SIGFRAME_ALIGN: usize = 16;
const RV64_SIGFRAME_MAGIC: u64 = 0x5458_5632_5349_4732; // "TXV2SIG2" (bumped version tag)
const RV64_SIGFRAME_VERSION: u32 = 3;
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
struct LinuxFpStateUnionRv64 {
    q_regs: [u64; 64],
    fcsr: u32,
    reserved: [u32; 3],
}

unsafe impl Pod for LinuxFpStateUnionRv64 {}

impl LinuxFpStateUnionRv64 {
    fn from_user_fp(fp: &tx_hal::UserFpContext) -> Self {
        let mut out = Self {
            q_regs: [0; 64],
            fcsr: 0,
            reserved: [0; 3],
        };
        out.q_regs[..32].copy_from_slice(&fp.regs);
        out.fcsr = fp.fcsr;
        out
    }

    fn to_user_fp(self, template: tx_hal::UserFpContext) -> tx_hal::UserFpContext {
        let mut fp = template;
        fp.regs.copy_from_slice(&self.q_regs[..32]);
        fp.fcsr = self.fcsr;
        fp
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxMcontextRv64 {
    gregs: [u64; 32],
    fpregs: LinuxFpStateUnionRv64,
}

unsafe impl Pod for LinuxMcontextRv64 {}

impl LinuxMcontextRv64 {
    fn from_user_context(ctx: &UserTrapContext) -> Self {
        let mut out = Self {
            gregs: [0; 32],
            fpregs: LinuxFpStateUnionRv64::from_user_fp(&ctx.fp),
        };
        out.gregs[0] = ctx.pc as u64;
        for i in 1..32 {
            out.gregs[i] = ctx.regs[i] as u64;
        }
        out
    }

    fn to_user_context(self, template_fp: tx_hal::UserFpContext) -> UserTrapContext {
        let mut ctx = UserTrapContext::empty();
        ctx.pc = self.gregs[0] as usize;
        for i in 1..32 {
            ctx.regs[i] = self.gregs[i] as usize;
        }
        ctx.fp = self.fpregs.to_user_fp(template_fp);
        ctx
    }
}

/// Linux-compatible `ucontext_t` for RV64.
///
/// Layout matches musl's `arch/riscv64/bits/signal.h`, including the
/// full `mcontext_t { __gregs, __fpregs }` tail. Musl's pthread cancel
/// handler reads and rewrites `uc_mcontext.__gregs[REG_PC]`.
#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxUcontextRv64 {
    uc_flags: u64, //   8 bytes, offset   0
    uc_link: u64,  //   8 bytes, offset   8
    // stack_t: { ss_sp, ss_flags+pad, ss_size }
    uc_stack_sp: u64,    //   8 bytes, offset  16
    uc_stack_flags: u32, //   4 bytes, offset  24
    _pad_stack: u32,     //   4 bytes, offset  28
    uc_stack_size: u64,  //   8 bytes, offset  32
    // sigset_t: 128 bytes (musl uses 1024-bit sigset)
    uc_sigmask: [u8; 128], // 128 bytes, offset  40
    uc_mcontext: LinuxMcontextRv64,
}

unsafe impl Pod for LinuxUcontextRv64 {}

impl LinuxUcontextRv64 {
    const ZERO: Self = Self {
        uc_flags: 0,
        uc_link: 0,
        uc_stack_sp: 0,
        uc_stack_flags: 0,
        _pad_stack: 0,
        uc_stack_size: 0,
        uc_sigmask: [0; 128],
        uc_mcontext: LinuxMcontextRv64 {
            gregs: [0; 32],
            fpregs: LinuxFpStateUnionRv64 {
                q_regs: [0; 64],
                fcsr: 0,
                reserved: [0; 3],
            },
        },
    };

    /// Build from a `UserTrapContext` and signal mask.
    fn from_user_context(ctx: &UserTrapContext, mask_bits: u64) -> Self {
        let mut uc = Self::ZERO;
        uc.uc_mcontext = LinuxMcontextRv64::from_user_context(ctx);
        // Store the signal mask in the first 8 bytes of uc_sigmask.
        uc.uc_sigmask[..8].copy_from_slice(&mask_bits.to_ne_bytes());
        uc
    }

    /// Read back the signal mask from uc_sigmask.
    fn signal_mask_bits(self) -> u64 {
        u64::from_ne_bytes(self.uc_sigmask[..8].try_into().unwrap())
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Rv64SignalFrame {
    // --- txKernel validation header ---
    magic: u64,
    version: u32,
    frame_size: u32,
    sig_no: u32,
    _reserved0: u32,
    flags: u64,
    // FP + status state from UserTrapContext (not part of the Linux
    // ucontext ABI, but needed for correct FP restore on sigreturn).
    saved_fp: tx_hal::UserFpContext,
    saved_status: u64,
    // --- siginfo_t (128 bytes, pointed to by a1) ---
    siginfo: tx_hal::UserSigInfoAbi,
    // --- Linux-compatible ucontext_t (pointed to by a2) ---
    ucontext: LinuxUcontextRv64,
    // --- sigreturn trampoline ---
    trampoline: [u32; 2],
}

unsafe impl Pod for Rv64SignalFrame {}

impl Rv64SignalFrame {
    fn new(tf: &TrapFrameMut<'_>, setup: &SignalFrameWrite) -> Self {
        let captured = tf.capture_user_context();
        Self {
            magic: RV64_SIGFRAME_MAGIC,
            version: RV64_SIGFRAME_VERSION,
            frame_size: size_of::<Self>() as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            saved_fp: captured.fp,
            saved_status: captured.status as u64,
            siginfo: setup.siginfo,
            ucontext: LinuxUcontextRv64::from_user_context(&captured, setup.old_mask.bits),
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

    /// Reconstruct a full `UserTrapContext` by merging the Linux-format
    /// GPR+PC from the ucontext with the FP/status fields saved in the
    /// txKernel header.
    fn to_full_user_context(self) -> UserTrapContext {
        let mut ctx = self.ucontext.uc_mcontext.to_user_context(self.saved_fp);
        ctx.status = self.saved_status as usize;
        ctx
    }

    fn saved_frame(self, user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo> {
        self.validate(user_sp)?;
        Ok(SavedSignalFrame {
            saved_mask: UserSignalMaskAbi {
                bits: self.ucontext.signal_mask_bits(),
            },
            user_context: self.to_full_user_context(),
        })
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
            let frame_ptr: *const Rv64SignalFrame = &frame;
            let bytes =
                core::slice::from_raw_parts(frame_ptr.cast::<u8>(), size_of::<Rv64SignalFrame>());
            board_copy_to_user(UserPtr::<u8>::new(user_frame.addr()), bytes)?;
        }

        let siginfo_addr = frame_addr + offset_of!(Rv64SignalFrame, siginfo);
        let ucontext_addr = frame_addr + offset_of!(Rv64SignalFrame, ucontext);
        let trampoline_pc = frame_addr + offset_of!(Rv64SignalFrame, trampoline);
        let return_pc = if setup.restorer_pc.addr() != 0 {
            setup.restorer_pc.addr()
        } else {
            trampoline_pc
        };

        tf.set_pc(VirtAddr(setup.handler_pc.addr()));
        tf.set_sp(VirtAddr(frame_addr));
        tf.set_signal_handler_regs(SignalHandlerRegs {
            return_pc: VirtAddr(return_pc),
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

        frame.saved_frame(user_sp)
    }

    fn signal_frame_size() -> usize {
        size_of::<Rv64SignalFrame>()
    }

    fn decode_signal_frame_bytes(
        user_sp: UserPtr<u8>,
        bytes: &[u8],
    ) -> Result<SavedSignalFrame, FaultInfo> {
        if bytes.len() != size_of::<Rv64SignalFrame>() {
            return Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: false,
            });
        }
        let mut frame = core::mem::MaybeUninit::<Rv64SignalFrame>::uninit();
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                frame.as_mut_ptr().cast::<u8>(),
                size_of::<Rv64SignalFrame>(),
            );
            frame.assume_init()
        }
        .saved_frame(user_sp)
    }

    fn restore_signal_frame(mut tf: TrapFrameMut<'_>, frame: &SavedSignalFrame) {
        tf.restore_user_context(&frame.user_context);
    }

    fn rewind_syscall_pc(mut tf: TrapFrameMut<'_>) {
        tf.rewind_pc(4);
    }

    fn prepare_signal_frame(
        ctx: &UserTrapContext,
        setup: &SignalFrameWrite,
    ) -> Result<(UserTrapContext, SignalFrameBytes), FaultInfo> {
        let frame_size = size_of::<Rv64SignalFrame>();
        let user_sp = UserPtr::<u8>::new(ctx.regs[2]); // sp = x2

        let Some(unrounded) = user_sp.addr().checked_sub(frame_size) else {
            return Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: true,
                instruction: false,
                from_user: false,
            });
        };
        let frame_addr = align_down(unrounded, RV64_SIGFRAME_ALIGN);

        // Build the signal frame with Linux-compatible ucontext layout.
        let frame = Rv64SignalFrame {
            magic: RV64_SIGFRAME_MAGIC,
            version: RV64_SIGFRAME_VERSION,
            frame_size: frame_size as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            saved_fp: ctx.fp,
            saved_status: ctx.status as u64,
            siginfo: setup.siginfo,
            ucontext: LinuxUcontextRv64::from_user_context(ctx, setup.old_mask.bits),
            trampoline: RV64_SIGRETURN_TRAMPOLINE,
        };

        // SAFETY: Rv64SignalFrame is Pod.
        let frame_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&frame as *const Rv64SignalFrame as *const u8, frame_size)
        };

        let trampoline_pc = frame_addr + offset_of!(Rv64SignalFrame, trampoline);
        let return_pc = if setup.restorer_pc.addr() != 0 {
            setup.restorer_pc.addr()
        } else {
            trampoline_pc
        };

        // Build handler-entry UserTrapContext.
        let mut handler_ctx = *ctx;
        handler_ctx.pc = setup.handler_pc.addr();
        handler_ctx.regs[2] = frame_addr; // sp
        handler_ctx.regs[1] = return_pc; // ra
        let siginfo_addr = frame_addr + offset_of!(Rv64SignalFrame, siginfo);
        let ucontext_addr = frame_addr + offset_of!(Rv64SignalFrame, ucontext);
        handler_ctx.regs[10] = setup.sig_no as usize; // a0
        handler_ctx.regs[11] = siginfo_addr; // a1
        handler_ctx.regs[12] = ucontext_addr; // a2

        Ok((handler_ctx, SignalFrameBytes::from_slice(frame_bytes)))
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

    #[test]
    fn prepared_signal_frame_uses_explicit_restorer_when_supplied() {
        let mut context = UserTrapContext::empty();
        context.regs[2] = 0x8000;
        let setup = SignalFrameWrite {
            stack_top: UserPtr::new(context.regs[2]),
            sig_no: 10,
            siginfo: tx_hal::UserSigInfoAbi::ZERO,
            old_mask: tx_hal::UserSignalMaskAbi::EMPTY,
            flags: tx_hal::UserSaFlagsAbi::EMPTY,
            handler_pc: UserPtr::new(0x5000),
            restorer_pc: UserPtr::new(0x6000),
        };

        let (handler_ctx, _) =
            <Platform as SignalFrameIf>::prepare_signal_frame(&context, &setup).expect("prepare");
        assert_eq!(handler_ctx.pc, 0x5000);
        assert_eq!(handler_ctx.regs[1], 0x6000);
    }

    /// Verify that `uc_mcontext.__gregs[0]` (the PC field musl reads via
    /// `MC_PC`) sits at musl's aligned RV64 `ucontext_t` location.
    #[test]
    fn ucontext_gregs0_at_linux_abi_offset() {
        assert_eq!(
            offset_of!(LinuxUcontextRv64, uc_mcontext),
            176,
            "uc_mcontext must be 16-byte aligned after uc_sigmask"
        );
        assert_eq!(
            offset_of!(LinuxMcontextRv64, gregs),
            0,
            "mcontext_t.__gregs must start at the mcontext base"
        );
        assert_eq!(
            offset_of!(LinuxMcontextRv64, fpregs),
            256,
            "mcontext_t.__fpregs must follow the 32 gregs"
        );
        assert_eq!(
            size_of::<LinuxFpStateUnionRv64>(),
            528,
            "musl riscv64 fpregset_t is the aligned Q-extension union"
        );
        assert_eq!(
            size_of::<LinuxUcontextRv64>(),
            960,
            "ucontext_t must include the full musl mcontext_t tail"
        );
    }

    /// Round-trip: UserTrapContext → LinuxUcontextRv64 → UserTrapContext
    /// preserves PC and GPRs.
    #[test]
    fn ucontext_round_trip_preserves_pc_and_regs() {
        let mut ctx = UserTrapContext::empty();
        ctx.pc = 0x8000_1234;
        ctx.regs[1] = 0xAAAA; // ra
        ctx.regs[2] = 0xBBBB; // sp
        ctx.regs[10] = 0xCCCC; // a0
        ctx.regs[31] = 0xDDDD; // t6

        ctx.fp.regs[3] = 0xEEEE;
        ctx.fp.fcsr = 7;

        let uc = LinuxUcontextRv64::from_user_context(&ctx, 0x42);
        assert_eq!(uc.uc_mcontext.gregs[0], 0x8000_1234, "gregs[0] must be PC");
        assert_eq!(uc.uc_mcontext.gregs[1], 0xAAAA, "gregs[1] must be ra");
        assert_eq!(uc.uc_mcontext.gregs[10], 0xCCCC, "gregs[10] must be a0");
        assert_eq!(uc.uc_mcontext.fpregs.q_regs[3], 0xEEEE);
        assert_eq!(uc.uc_mcontext.fpregs.fcsr, 7);

        let restored = uc
            .uc_mcontext
            .to_user_context(tx_hal::UserFpContext::empty());
        assert_eq!(restored.pc, 0x8000_1234);
        assert_eq!(restored.regs[0], 0, "x0 must always be zero");
        assert_eq!(restored.regs[1], 0xAAAA);
        assert_eq!(restored.regs[2], 0xBBBB);
        assert_eq!(restored.regs[10], 0xCCCC);
        assert_eq!(restored.regs[31], 0xDDDD);
        assert_eq!(restored.fp.regs[3], 0xEEEE);
        assert_eq!(restored.fp.fcsr, 7);

        assert_eq!(uc.signal_mask_bits(), 0x42);
    }
}
