use super::*;

const LDH_OP: u32 = 0xa1;
const LDHU_OP: u32 = 0xa9;
const LDW_OP: u32 = 0xa2;
const LDWU_OP: u32 = 0xaa;
const LDD_OP: u32 = 0xa3;
const STH_OP: u32 = 0xa5;
const STW_OP: u32 = 0xa6;
const STD_OP: u32 = 0xa7;

const LDPTRW_OP: u32 = 0x24;
const LDPTRD_OP: u32 = 0x26;
const STPTRW_OP: u32 = 0x25;
const STPTRD_OP: u32 = 0x27;

const LDXH_OP: u32 = 0x7008;
const LDXHU_OP: u32 = 0x7048;
const LDXW_OP: u32 = 0x7010;
const LDXWU_OP: u32 = 0x7050;
const LDXD_OP: u32 = 0x7018;
const STXH_OP: u32 = 0x7028;
const STXW_OP: u32 = 0x7030;
const STXD_OP: u32 = 0x7038;

const FLDS_OP: u32 = 0xac;
const FLDD_OP: u32 = 0xae;
const FSTS_OP: u32 = 0xad;
const FSTD_OP: u32 = 0xaf;

const FLDXS_OP: u32 = 0x7060;
const FLDXD_OP: u32 = 0x7068;
const FSTXS_OP: u32 = 0x7070;
const FSTXD_OP: u32 = 0x7078;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnalignedOutcome {
    Emulated,
    Unsupported,
    Fault(FaultInfo),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegClass {
    Gpr,
    Fpr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccessKind {
    Load,
    Store,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedUnaligned {
    kind: AccessKind,
    reg_class: RegClass,
    reg: usize,
    width: usize,
    signed: bool,
}

pub(crate) fn emulate_user_unaligned(frame: &mut La64TrapFrame) -> UnalignedOutcome {
    let Ok(inst) = read_user_u32(frame.era) else {
        return UnalignedOutcome::Fault(FaultInfo {
            address: VirtAddr(frame.era),
            write: false,
            instruction: true,
            from_user: true,
        });
    };

    let Some(decoded) = decode_unaligned_access(inst) else {
        return UnalignedOutcome::Unsupported;
    };

    match decoded.kind {
        AccessKind::Load => {
            let mut raw = 0;
            if let Err(fault) = read_user_value(frame.badv, decoded.width, &mut raw) {
                return UnalignedOutcome::Fault(user_data_fault(fault, false));
            }

            let value = if decoded.signed {
                sign_extend(raw, decoded.width)
            } else {
                mask_width(raw, decoded.width)
            };
            write_register(frame, decoded, value);
        }
        AccessKind::Store => {
            let value = read_register(frame, decoded);
            if let Err(fault) = write_user_value(frame.badv, decoded.width, value) {
                return UnalignedOutcome::Fault(user_data_fault(fault, true));
            }
        }
    }

    frame.era = frame.era.saturating_add(4);
    UnalignedOutcome::Emulated
}

/// Kernel-mode unaligned access emulation. The LA264 (LS2K1000) has
/// no hardware unaligned access support — QEMU silently emulates it,
/// so the first real-board boot died with ALE inside
/// `claim_zero_frame` (2026-07-04 first flight). Kernel addresses sit
/// in the DMW windows and are directly dereferenceable, so both the
/// instruction fetch (era, always 4-byte aligned) and the data access
/// (badv, byte-assembled — single-byte ops can never fault ALE) are
/// plain volatile accesses; no fixup plumbing needed. Reuses the same
/// decoder and register accessors as the user-mode path.
pub(crate) fn emulate_kernel_unaligned(frame: &mut La64TrapFrame) -> UnalignedOutcome {
    let inst = unsafe { core::ptr::read_volatile(frame.era as *const u32) };

    let Some(decoded) = decode_unaligned_access(inst) else {
        return UnalignedOutcome::Unsupported;
    };

    match decoded.kind {
        AccessKind::Load => {
            let mut raw: u64 = 0;
            for offset in 0..decoded.width {
                let byte = unsafe { core::ptr::read_volatile((frame.badv + offset) as *const u8) };
                raw |= (byte as u64) << (8 * offset);
            }
            let value = if decoded.signed {
                sign_extend(raw, decoded.width)
            } else {
                mask_width(raw, decoded.width)
            };
            write_register(frame, decoded, value);
        }
        AccessKind::Store => {
            let value = read_register(frame, decoded);
            for offset in 0..decoded.width {
                unsafe {
                    core::ptr::write_volatile(
                        (frame.badv + offset) as *mut u8,
                        (value >> (8 * offset)) as u8,
                    );
                }
            }
        }
    }

    frame.era = frame.era.saturating_add(4);
    UnalignedOutcome::Emulated
}

fn decode_unaligned_access(inst: u32) -> Option<DecodedUnaligned> {
    let reg = (inst & 0x1f) as usize;
    let op_22 = inst >> 22;
    let op_24 = inst >> 24;
    let op_15 = inst >> 15;

    let (kind, reg_class, width, signed) = if op_22 == LDD_OP || op_24 == LDPTRD_OP {
        (AccessKind::Load, RegClass::Gpr, 8, true)
    } else if op_22 == LDW_OP || op_24 == LDPTRW_OP {
        (AccessKind::Load, RegClass::Gpr, 4, true)
    } else if op_22 == LDWU_OP {
        (AccessKind::Load, RegClass::Gpr, 4, false)
    } else if op_22 == LDH_OP {
        (AccessKind::Load, RegClass::Gpr, 2, true)
    } else if op_22 == LDHU_OP {
        (AccessKind::Load, RegClass::Gpr, 2, false)
    } else if op_22 == STD_OP || op_24 == STPTRD_OP {
        (AccessKind::Store, RegClass::Gpr, 8, false)
    } else if op_22 == STW_OP || op_24 == STPTRW_OP {
        (AccessKind::Store, RegClass::Gpr, 4, false)
    } else if op_22 == STH_OP {
        (AccessKind::Store, RegClass::Gpr, 2, false)
    } else if op_22 == FLDD_OP {
        (AccessKind::Load, RegClass::Fpr, 8, false)
    } else if op_22 == FLDS_OP {
        (AccessKind::Load, RegClass::Fpr, 4, false)
    } else if op_22 == FSTD_OP {
        (AccessKind::Store, RegClass::Fpr, 8, false)
    } else if op_22 == FSTS_OP {
        (AccessKind::Store, RegClass::Fpr, 4, false)
    } else if op_15 == LDXD_OP {
        (AccessKind::Load, RegClass::Gpr, 8, true)
    } else if op_15 == LDXW_OP {
        (AccessKind::Load, RegClass::Gpr, 4, true)
    } else if op_15 == LDXWU_OP {
        (AccessKind::Load, RegClass::Gpr, 4, false)
    } else if op_15 == LDXH_OP {
        (AccessKind::Load, RegClass::Gpr, 2, true)
    } else if op_15 == LDXHU_OP {
        (AccessKind::Load, RegClass::Gpr, 2, false)
    } else if op_15 == STXD_OP {
        (AccessKind::Store, RegClass::Gpr, 8, false)
    } else if op_15 == STXW_OP {
        (AccessKind::Store, RegClass::Gpr, 4, false)
    } else if op_15 == STXH_OP {
        (AccessKind::Store, RegClass::Gpr, 2, false)
    } else if op_15 == FLDXD_OP {
        (AccessKind::Load, RegClass::Fpr, 8, false)
    } else if op_15 == FLDXS_OP {
        (AccessKind::Load, RegClass::Fpr, 4, false)
    } else if op_15 == FSTXD_OP {
        (AccessKind::Store, RegClass::Fpr, 8, false)
    } else if op_15 == FSTXS_OP {
        (AccessKind::Store, RegClass::Fpr, 4, false)
    } else {
        return None;
    };

    Some(DecodedUnaligned {
        kind,
        reg_class,
        reg,
        width,
        signed,
    })
}

fn read_register(frame: &La64TrapFrame, decoded: DecodedUnaligned) -> u64 {
    match decoded.reg_class {
        RegClass::Gpr => frame.r[decoded.reg] as u64,
        RegClass::Fpr => la64_read_fpr(decoded.reg),
    }
}

fn write_register(frame: &mut La64TrapFrame, decoded: DecodedUnaligned, value: u64) {
    match decoded.reg_class {
        RegClass::Gpr => {
            if decoded.reg != 0 {
                frame.r[decoded.reg] = value as usize;
            }
        }
        RegClass::Fpr => la64_write_fpr(decoded.reg, value),
    }
}

fn read_user_u32(addr: usize) -> Result<u32, FaultInfo> {
    let mut value = 0u32;
    unsafe {
        super::platform_impls::la64_copy_from_user_raw(
            core::ptr::addr_of_mut!(value) as *mut u8,
            UserPtr::<u8>::new(addr),
            core::mem::size_of::<u32>(),
        )?;
    }
    Ok(value)
}

fn read_user_value(addr: usize, width: usize, out: &mut u64) -> Result<(), FaultInfo> {
    let mut bytes = [0u8; 8];
    unsafe {
        super::platform_impls::la64_copy_from_user_raw(
            bytes.as_mut_ptr(),
            UserPtr::<u8>::new(addr),
            width,
        )?;
    }
    *out = u64::from_le_bytes(bytes);
    Ok(())
}

fn write_user_value(addr: usize, width: usize, value: u64) -> Result<(), FaultInfo> {
    let bytes = value.to_le_bytes();
    unsafe {
        super::platform_impls::la64_copy_to_user_raw(
            UserPtr::<u8>::new(addr),
            bytes.as_ptr(),
            width,
        )
    }
}

fn user_data_fault(fault: FaultInfo, write: bool) -> FaultInfo {
    FaultInfo {
        address: fault.address,
        write,
        instruction: false,
        from_user: true,
    }
}

fn mask_width(value: u64, width: usize) -> u64 {
    match width {
        2 => value & 0xffff,
        4 => value & 0xffff_ffff,
        8 => value,
        _ => 0,
    }
}

fn sign_extend(value: u64, width: usize) -> u64 {
    match width {
        2 => (value as u16 as i16 as i64) as u64,
        4 => (value as u32 as i32 as i64) as u64,
        8 => value,
        _ => 0,
    }
}

#[cfg(target_arch = "loongarch64")]
fn la64_read_fpr(reg: usize) -> u64 {
    let value: u64;
    unsafe {
        match reg {
            0 => core::arch::asm!("movfr2gr.d {value}, $f0", value = out(reg) value),
            1 => core::arch::asm!("movfr2gr.d {value}, $f1", value = out(reg) value),
            2 => core::arch::asm!("movfr2gr.d {value}, $f2", value = out(reg) value),
            3 => core::arch::asm!("movfr2gr.d {value}, $f3", value = out(reg) value),
            4 => core::arch::asm!("movfr2gr.d {value}, $f4", value = out(reg) value),
            5 => core::arch::asm!("movfr2gr.d {value}, $f5", value = out(reg) value),
            6 => core::arch::asm!("movfr2gr.d {value}, $f6", value = out(reg) value),
            7 => core::arch::asm!("movfr2gr.d {value}, $f7", value = out(reg) value),
            8 => core::arch::asm!("movfr2gr.d {value}, $f8", value = out(reg) value),
            9 => core::arch::asm!("movfr2gr.d {value}, $f9", value = out(reg) value),
            10 => core::arch::asm!("movfr2gr.d {value}, $f10", value = out(reg) value),
            11 => core::arch::asm!("movfr2gr.d {value}, $f11", value = out(reg) value),
            12 => core::arch::asm!("movfr2gr.d {value}, $f12", value = out(reg) value),
            13 => core::arch::asm!("movfr2gr.d {value}, $f13", value = out(reg) value),
            14 => core::arch::asm!("movfr2gr.d {value}, $f14", value = out(reg) value),
            15 => core::arch::asm!("movfr2gr.d {value}, $f15", value = out(reg) value),
            16 => core::arch::asm!("movfr2gr.d {value}, $f16", value = out(reg) value),
            17 => core::arch::asm!("movfr2gr.d {value}, $f17", value = out(reg) value),
            18 => core::arch::asm!("movfr2gr.d {value}, $f18", value = out(reg) value),
            19 => core::arch::asm!("movfr2gr.d {value}, $f19", value = out(reg) value),
            20 => core::arch::asm!("movfr2gr.d {value}, $f20", value = out(reg) value),
            21 => core::arch::asm!("movfr2gr.d {value}, $f21", value = out(reg) value),
            22 => core::arch::asm!("movfr2gr.d {value}, $f22", value = out(reg) value),
            23 => core::arch::asm!("movfr2gr.d {value}, $f23", value = out(reg) value),
            24 => core::arch::asm!("movfr2gr.d {value}, $f24", value = out(reg) value),
            25 => core::arch::asm!("movfr2gr.d {value}, $f25", value = out(reg) value),
            26 => core::arch::asm!("movfr2gr.d {value}, $f26", value = out(reg) value),
            27 => core::arch::asm!("movfr2gr.d {value}, $f27", value = out(reg) value),
            28 => core::arch::asm!("movfr2gr.d {value}, $f28", value = out(reg) value),
            29 => core::arch::asm!("movfr2gr.d {value}, $f29", value = out(reg) value),
            30 => core::arch::asm!("movfr2gr.d {value}, $f30", value = out(reg) value),
            31 => core::arch::asm!("movfr2gr.d {value}, $f31", value = out(reg) value),
            _ => value = 0,
        }
    }
    value
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_read_fpr(_reg: usize) -> u64 {
    0
}

#[cfg(target_arch = "loongarch64")]
fn la64_write_fpr(reg: usize, value: u64) {
    unsafe {
        match reg {
            0 => core::arch::asm!("movgr2fr.d $f0, {value}", value = in(reg) value),
            1 => core::arch::asm!("movgr2fr.d $f1, {value}", value = in(reg) value),
            2 => core::arch::asm!("movgr2fr.d $f2, {value}", value = in(reg) value),
            3 => core::arch::asm!("movgr2fr.d $f3, {value}", value = in(reg) value),
            4 => core::arch::asm!("movgr2fr.d $f4, {value}", value = in(reg) value),
            5 => core::arch::asm!("movgr2fr.d $f5, {value}", value = in(reg) value),
            6 => core::arch::asm!("movgr2fr.d $f6, {value}", value = in(reg) value),
            7 => core::arch::asm!("movgr2fr.d $f7, {value}", value = in(reg) value),
            8 => core::arch::asm!("movgr2fr.d $f8, {value}", value = in(reg) value),
            9 => core::arch::asm!("movgr2fr.d $f9, {value}", value = in(reg) value),
            10 => core::arch::asm!("movgr2fr.d $f10, {value}", value = in(reg) value),
            11 => core::arch::asm!("movgr2fr.d $f11, {value}", value = in(reg) value),
            12 => core::arch::asm!("movgr2fr.d $f12, {value}", value = in(reg) value),
            13 => core::arch::asm!("movgr2fr.d $f13, {value}", value = in(reg) value),
            14 => core::arch::asm!("movgr2fr.d $f14, {value}", value = in(reg) value),
            15 => core::arch::asm!("movgr2fr.d $f15, {value}", value = in(reg) value),
            16 => core::arch::asm!("movgr2fr.d $f16, {value}", value = in(reg) value),
            17 => core::arch::asm!("movgr2fr.d $f17, {value}", value = in(reg) value),
            18 => core::arch::asm!("movgr2fr.d $f18, {value}", value = in(reg) value),
            19 => core::arch::asm!("movgr2fr.d $f19, {value}", value = in(reg) value),
            20 => core::arch::asm!("movgr2fr.d $f20, {value}", value = in(reg) value),
            21 => core::arch::asm!("movgr2fr.d $f21, {value}", value = in(reg) value),
            22 => core::arch::asm!("movgr2fr.d $f22, {value}", value = in(reg) value),
            23 => core::arch::asm!("movgr2fr.d $f23, {value}", value = in(reg) value),
            24 => core::arch::asm!("movgr2fr.d $f24, {value}", value = in(reg) value),
            25 => core::arch::asm!("movgr2fr.d $f25, {value}", value = in(reg) value),
            26 => core::arch::asm!("movgr2fr.d $f26, {value}", value = in(reg) value),
            27 => core::arch::asm!("movgr2fr.d $f27, {value}", value = in(reg) value),
            28 => core::arch::asm!("movgr2fr.d $f28, {value}", value = in(reg) value),
            29 => core::arch::asm!("movgr2fr.d $f29, {value}", value = in(reg) value),
            30 => core::arch::asm!("movgr2fr.d $f30, {value}", value = in(reg) value),
            31 => core::arch::asm!("movgr2fr.d $f31, {value}", value = in(reg) value),
            _ => {}
        }
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_write_fpr(_reg: usize, _value: u64) {}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rd_inst(op: u32, rd: u32) -> u32 {
        (op << 22) | rd
    }

    const fn rd_ptr_inst(op: u32, rd: u32) -> u32 {
        (op << 24) | rd
    }

    const fn rd_index_inst(op: u32, rd: u32) -> u32 {
        (op << 15) | rd
    }

    #[test]
    fn decodes_integer_immediate_pointer_and_indexed_load_store() {
        assert_eq!(
            decode_unaligned_access(rd_inst(LDH_OP, 3)),
            Some(DecodedUnaligned {
                kind: AccessKind::Load,
                reg_class: RegClass::Gpr,
                reg: 3,
                width: 2,
                signed: true,
            })
        );
        assert_eq!(
            decode_unaligned_access(rd_ptr_inst(STPTRD_OP, 4)),
            Some(DecodedUnaligned {
                kind: AccessKind::Store,
                reg_class: RegClass::Gpr,
                reg: 4,
                width: 8,
                signed: false,
            })
        );
        assert_eq!(
            decode_unaligned_access(rd_index_inst(LDXWU_OP, 5)),
            Some(DecodedUnaligned {
                kind: AccessKind::Load,
                reg_class: RegClass::Gpr,
                reg: 5,
                width: 4,
                signed: false,
            })
        );
    }

    #[test]
    fn decodes_floating_immediate_and_indexed_load_store() {
        assert_eq!(
            decode_unaligned_access(rd_inst(FLDD_OP, 6)),
            Some(DecodedUnaligned {
                kind: AccessKind::Load,
                reg_class: RegClass::Fpr,
                reg: 6,
                width: 8,
                signed: false,
            })
        );
        assert_eq!(
            decode_unaligned_access(rd_index_inst(FSTXS_OP, 7)),
            Some(DecodedUnaligned {
                kind: AccessKind::Store,
                reg_class: RegClass::Fpr,
                reg: 7,
                width: 4,
                signed: false,
            })
        );
    }

    #[test]
    fn sign_extension_and_width_masks_match_la64_load_semantics() {
        assert_eq!(sign_extend(0x8001, 2), 0xffff_ffff_ffff_8001);
        assert_eq!(sign_extend(0x8000_0001, 4), 0xffff_ffff_8000_0001);
        assert_eq!(mask_width(0xffff_ffff_8000_0001, 4), 0x8000_0001);
        assert_eq!(mask_width(0x1234_5678_9abc_def0, 8), 0x1234_5678_9abc_def0);
    }

    #[test]
    fn user_unaligned_data_faults_preserve_address_and_restore_user_provenance() {
        let raw_fault = FaultInfo {
            address: VirtAddr(0x4925_000),
            write: false,
            instruction: true,
            from_user: false,
        };

        assert_eq!(
            user_data_fault(raw_fault, false),
            FaultInfo {
                address: VirtAddr(0x4925_000),
                write: false,
                instruction: false,
                from_user: true,
            }
        );
        assert_eq!(
            user_data_fault(raw_fault, true),
            FaultInfo {
                address: VirtAddr(0x4925_000),
                write: true,
                instruction: false,
                from_user: true,
            }
        );
    }

    #[test]
    fn gpr_write_ignores_r0_and_updates_other_registers() {
        let mut frame = La64TrapFrame {
            r: [0; 32],
            estat: LA64_ECODE_ALE << LA64_ESTAT_ECODE_SHIFT,
            era: 0x1000,
            badv: 0x2001,
            crmd: 0,
            prmd: LA64_PRMD_PPLV_USER | LA64_PRMD_PIE,
        };
        write_register(
            &mut frame,
            DecodedUnaligned {
                kind: AccessKind::Load,
                reg_class: RegClass::Gpr,
                reg: 0,
                width: 8,
                signed: false,
            },
            0x55,
        );
        write_register(
            &mut frame,
            DecodedUnaligned {
                kind: AccessKind::Load,
                reg_class: RegClass::Gpr,
                reg: 3,
                width: 8,
                signed: false,
            },
            0x66,
        );

        assert_eq!(frame.r[0], 0);
        assert_eq!(frame.r[3], 0x66);
    }
}
