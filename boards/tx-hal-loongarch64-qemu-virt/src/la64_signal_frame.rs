//! LA64 signal frame: the ABI record pushed onto the user stack when a signal
//! handler is delivered (magic/version/trampoline guarded), plus its
//! construction from a trapped user context and validation on `sigreturn`.

use super::*;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub(crate) struct La64SignalFrame {
    magic: u64,
    version: u32,
    frame_size: u32,
    sig_no: u32,
    _reserved0: u32,
    flags: u64,
    pub(crate) siginfo: tx_hal::UserSigInfoAbi,
    pub(crate) saved_mask: UserSignalMaskAbi,
    // Includes full GPR + PC + status and UserFpContext payload.
    pub(crate) user_context: UserTrapContext,
    pub(crate) trampoline: [u32; 2],
}

unsafe impl Pod for La64SignalFrame {}

impl La64SignalFrame {
    pub(crate) fn new_from_context(context: &UserTrapContext, setup: &SignalFrameWrite) -> Self {
        Self {
            magic: LA64_SIGFRAME_MAGIC,
            version: LA64_SIGFRAME_VERSION,
            frame_size: core::mem::size_of::<Self>() as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            siginfo: setup.siginfo,
            saved_mask: setup.old_mask,
            user_context: *context,
            trampoline: LA64_SIGRETURN_TRAMPOLINE,
        }
    }

    pub(crate) fn validate(&self, user_sp: UserPtr<u8>) -> Result<(), FaultInfo> {
        if self.magic == LA64_SIGFRAME_MAGIC
            && self.version == LA64_SIGFRAME_VERSION
            && self.frame_size as usize == core::mem::size_of::<Self>()
            && self.trampoline == LA64_SIGRETURN_TRAMPOLINE
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
