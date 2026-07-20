//! LA64 QEMU-virt / LS2K1000 constant pool: memory map, CSR / PTE / CRMD bit
//! definitions, exception (Ecode/EsubCode) numbering, trap-frame register
//! indices, signal-frame layout, and interrupt-controller offsets. Split out of
//! `lib.rs` and re-exported (`pub(crate) use la64_consts::*`) so every module's
//! `use super::*` keeps resolving them unchanged.

pub(crate) const QEMU_LA64_RAM_BASE: usize = 0;
// Low-RAM window only. On QEMU `virt`, [0, 256 MiB) is low RAM; the rest of the
// guest RAM (anything past `-m 256M`) lives in the *high* aperture at
// `QEMU_LA64_HIGH_RAM_BASE`, separated by the MMIO/PCIe hole. `RAM_END` is the
// low-RAM/IO-hole boundary and the no-firmware fallback size — NOT the total.
pub(crate) const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
pub(crate) const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
// QEMU `virt` places high RAM (guest memory beyond the low 256 MiB) at this
// physical base, above the MMIO/PCIe apertures. Documents the layout the DTB
// high-RAM region (recovered by the DMW-wide direct map) lands in; referenced
// by the direct-map coverage test.
#[allow(dead_code)]
pub(crate) const QEMU_LA64_HIGH_RAM_BASE: usize = 0x9000_0000;
// The cached DMW window (VSEG 0x9) hardware-maps the *entire* physical address
// space at a fixed offset, so the kernel direct map spans all of it — high RAM
// is reachable with no per-page mappings. This bounds the direct-map bookkeeping
// (`direct_map_covers_phys_end`, `extend_direct_map`); the actual frame-metadata
// span is still carved from the real firmware memory map, not from this size.
pub(crate) const QEMU_LA64_DIRECT_MAP_SIZE: usize = LA64_PHYS_ADDR_MASK + 1;
// Must match KERNEL_LOAD_BASE in linker-la64-qemu-virt.ld: the
// unified QEMU-virt/LS2K1000 load base in the high RAM region.
pub(crate) const QEMU_LA64_KERNEL_LOAD_BASE: usize = 0x9000_0000;
// No-firmware fallback: with nothing describing RAM, trust only a
// conservative 256 MiB window from the load base — the kernel is
// demonstrably executing there, and every supported machine (QEMU
// virt with -m >= 768M, LS2K1000 DDR) backs at least this much at
// 0x9000_0000. The board's exact static map replaces this in P4.2.
pub(crate) const QEMU_LA64_FALLBACK_HIGH_USABLE_END: usize = QEMU_LA64_KERNEL_LOAD_BASE + 0x1000_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) const QEMU_LA64_PCH_PIC_BASE: usize = 0x1000_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) const QEMU_LA64_GED_REG_BASE: usize = 0x100e_001c;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) const QEMU_LA64_GED_SLEEP_CTL: usize = QEMU_LA64_GED_REG_BASE;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) const QEMU_LA64_GED_SLEEP_VALUE_S5: u8 = (5 << 2) | (1 << 5);
pub(crate) const QEMU_LA64_GSI_BASE: u32 = 64;
pub(crate) const QEMU_LA64_PCH_PIC_IRQS: u32 = 64;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const QEMU_LA64_UART0_IRQ: u32 = 66;
pub(crate) const QEMU_LA64_PCIE_ECAM_BASE: usize = 0x2000_0000;
pub(crate) const QEMU_LA64_PCIE_ECAM_SIZE: usize = 0x0800_0000;
pub(crate) const QEMU_LA64_PCIE_MMIO32_BASE: usize = 0x4000_0000;
pub(crate) const QEMU_LA64_PCIE_MMIO32_SIZE: usize = 0x4000_0000;
pub(crate) const QEMU_LA64_PCH_MSI_BASE: usize = 0x2ff0_0000;
pub(crate) const QEMU_LA64_PCH_MSI_SIZE: usize = 0x8;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) const QEMU_LA64_FW_CFG_BASE: usize = 0x1e02_0000;
pub(crate) const QEMU_LA64_FDT_BASE: usize = 0x0010_0000;
pub(crate) const LA64_MAX_BOOT_CPUS: usize = 4;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_DEFAULT_POSSIBLE_CPUS: usize = LA64_MAX_BOOT_CPUS;
#[cfg(not(target_arch = "loongarch64"))]
pub(crate) const LA64_DEFAULT_POSSIBLE_CPUS: usize = 1;
pub(crate) const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
pub(crate) const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
pub(crate) const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
pub(crate) const LA64_CSR_CRMD: usize = 0x00;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_CSR_EUEN: usize = 0x02;
pub(crate) const LA64_CSR_ECFG: usize = 0x04;
pub(crate) const LA64_CSR_EENTRY: usize = 0x0c;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_CSR_KSAVE0: usize = 0x30;
pub(crate) const LA64_CSR_ASID: usize = 0x18;
pub(crate) const LA64_CSR_PGDL: usize = 0x19;
pub(crate) const LA64_CSR_PGDH: usize = 0x1a;
pub(crate) const LA64_CSR_PWCL: usize = 0x1c;
pub(crate) const LA64_CSR_PWCH: usize = 0x1d;
pub(crate) const LA64_CSR_STLBPS: usize = 0x1e;
pub(crate) const LA64_CSR_TLBRENTRY: usize = 0x88;
pub(crate) const LA64_CSR_TLBREHI: usize = 0x8e;
pub(crate) const LA64_CSR_MERRENTRY: usize = 0x93;
pub(crate) const LA64_CSR_TCFG: usize = 0x41;
pub(crate) const LA64_CSR_TICLR: usize = 0x44;
pub(crate) const LA64_CRMD_IE: usize = 1 << 2;
pub(crate) const LA64_CRMD_PG: usize = 1 << 4;
pub(crate) const LA64_CRMD_DATF_CC: usize = 0b01 << 5;
pub(crate) const LA64_CRMD_DATM_CC: usize = 0b01 << 7;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_EUEN_FPE: usize = 1 << 0;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_EUEN_SXE: usize = 1 << 1;
#[cfg(target_arch = "loongarch64")]
pub(crate) const LA64_EUEN_ASXE: usize = 1 << 2;
pub(crate) const LA64_ASID_MASK: usize = 0x3ff;
pub(crate) const LA64_TCFG_ENABLE: usize = 1 << 0;
pub(crate) const LA64_TCFG_PERIODIC: usize = 1 << 1;
pub(crate) const LA64_TCFG_TICK_MASK: usize = !0x3;
pub(crate) const LA64_TICLR_CLEAR_TIMER: usize = 1 << 0;
pub(crate) const LA64_CPUCFG2_LLFTP: u32 = 1 << 14;
pub(crate) const LA64_CPUCFG2: usize = 0x2;
pub(crate) const LA64_CPUCFG4: usize = 0x4;
pub(crate) const LA64_CPUCFG5: usize = 0x5;
pub(crate) const LA64_ESTAT_IS_HWI_MASK: usize = 0xff << 2;
pub(crate) const LA64_ESTAT_IS_TIMER: usize = 1 << 11;
pub(crate) const LA64_ESTAT_IS_IPI: usize = 1 << 12;
pub(crate) const LA64_ESTAT_ECODE_SHIFT: usize = 16;
pub(crate) const LA64_ESTAT_ECODE_MASK: usize = 0x3f;
pub(crate) const LA64_ECODE_INT: usize = 0;
pub(crate) const LA64_ECODE_PIL: usize = 1;
pub(crate) const LA64_ECODE_PIS: usize = 2;
pub(crate) const LA64_ECODE_PIF: usize = 3;
pub(crate) const LA64_ECODE_PME: usize = 4;
pub(crate) const LA64_ECODE_PNR: usize = 5;
pub(crate) const LA64_ECODE_PNX: usize = 6;
pub(crate) const LA64_ECODE_PPI: usize = 7;
// Ecode numbering per the LoongArch reference manual (and Linux
// EXCCODE_*): ADE is Ecode 8 with EsubCode 0 (ADEF, fetch) / 1 (ADEM,
// memory access); ALE is Ecode 9. An earlier version of this table
// listed ADEF/ADEM as Ecodes 8/9 and pushed ALE to 10, so real-silicon
// unaligned traps (never generated by QEMU, which emulates unaligned
// access transparently) were misclassified and terminated the kernel
// on the LS2K1000 first flight.
pub(crate) const LA64_ESTAT_ESUBCODE_SHIFT: usize = 22;
pub(crate) const LA64_ESTAT_ESUBCODE_MASK: usize = 0x1ff;
pub(crate) const LA64_ECODE_ADE: usize = 8;
pub(crate) const LA64_ESUBCODE_ADEF: usize = 0;
pub(crate) const LA64_ECODE_ALE: usize = 9;
pub(crate) const LA64_ECODE_SYS: usize = 11;
pub(crate) const LA64_ECODE_BRK: usize = 12;
pub(crate) const LA64_ECODE_INE: usize = 13;
pub(crate) const LA64_ECODE_IPE: usize = 14;
pub(crate) const LA64_ECODE_FPD: usize = 15;
pub(crate) const LA64_ECODE_SXD: usize = 16;
pub(crate) const LA64_ECODE_ASXD: usize = 17;
pub(crate) const LA64_USER_TOP: usize = 0x0000_4000_0000_0000;
pub(crate) const LA64_PTE_PFN_MASK: u64 = ((1u64 << 48) - 1) & !((1u64 << 12) - 1);
pub(crate) const LA64_PTE_V: u64 = 1 << 0;
pub(crate) const LA64_PTE_A: u64 = 1 << 0;
pub(crate) const LA64_PTE_D: u64 = 1 << 1;
pub(crate) const LA64_PTE_PLV_USER: u64 = 0b11 << 2;
pub(crate) const LA64_PTE_MAT_SUC: u64 = 0b00 << 4;
pub(crate) const LA64_PTE_MAT_CC: u64 = 0b01 << 4;
pub(crate) const LA64_PTE_G: u64 = 1 << 6;
pub(crate) const LA64_PTE_PRESENT: u64 = 1 << 7;
pub(crate) const LA64_PTE_W: u64 = 1 << 8;
pub(crate) const LA64_PTE_M: u64 = 1 << 9;
pub(crate) const LA64_PTE_NR: u64 = 1 << 61;
pub(crate) const LA64_PTE_NX: u64 = 1 << 62;
pub(crate) const LA64_PTE_RPLV: u64 = 1 << 63;
pub(crate) const LA64_PRMD_PPLV_MASK: usize = 0x3;
pub(crate) const LA64_PRMD_PPLV_USER: usize = 0x3;
pub(crate) const LA64_PRMD_PIE: usize = 1 << 2;
pub(crate) const LA64_R_RA: usize = 1;
pub(crate) const LA64_R_TLS: usize = 2;
pub(crate) const LA64_R_SP: usize = 3;
pub(crate) const LA64_R_A0: usize = 4;
pub(crate) const LA64_R_A1: usize = 5;
pub(crate) const LA64_R_A2: usize = 6;
pub(crate) const LA64_R_A3: usize = 7;
pub(crate) const LA64_R_A4: usize = 8;
pub(crate) const LA64_R_A5: usize = 9;
pub(crate) const LA64_R_A7: usize = 11;
pub(crate) const LA64_SIGFRAME_ALIGN: usize = 16;
pub(crate) const LA64_SIGFRAME_MAGIC: u64 = 0x5458_5632_4c41_5331; // "TXV2LAS1"
pub(crate) const LA64_SIGFRAME_VERSION: u32 = 1;
pub(crate) const LA64_RT_SIGRETURN_SYSCALL: u32 = 139;
pub(crate) const LA64_ADDI_D_R11_ZERO_RT_SIGRETURN: u32 = la64_addi_d(11, 0, LA64_RT_SIGRETURN_SYSCALL);
pub(crate) const LA64_SYSCALL_0: u32 = 0x002b_0000;
pub(crate) const LA64_SIGRETURN_TRAMPOLINE: [u32; 2] = [LA64_ADDI_D_R11_ZERO_RT_SIGRETURN, LA64_SYSCALL_0];
pub(crate) const LA64_EIOINTC_BASE: usize = 0x1400;
pub(crate) const LA64_EIOINTC_ENABLE_START: usize = 0x200;
pub(crate) const LA64_EIOINTC_COREISR_START: usize = 0x400;
pub(crate) const LA64_EIOINTC_IRQS: u32 = 256;
pub(crate) const LA64_PCH_PIC_MASK_START: usize = 0x20;
pub(crate) const LA64_PCH_PIC_CLEAR_START: usize = 0x80;

pub(crate) const fn la64_addi_d(rd: u32, rj: u32, imm12: u32) -> u32 {
    0x02c0_0000 | ((imm12 & 0x0fff) << 10) | ((rj & 0x1f) << 5) | (rd & 0x1f)
}
