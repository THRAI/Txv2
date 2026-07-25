use tx_hal::{Arch, PlatformConfig};

/// Platform facts used to validate and lay out one ELF image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfLoadPolicy {
    pub arch: Arch,
    pub page_size: u64,
    pub user_top: u64,
    pub max_phdrs: u16,
    pub allow_interpreter: bool,
}

impl ElfLoadPolicy {
    pub fn for_platform<P: PlatformConfig>() -> Self {
        Self {
            arch: P::ARCH,
            page_size: P::PAGE_SIZE as u64,
            user_top: P::USER_TOP.0 as u64,
            max_phdrs: 64,
            allow_interpreter: true,
        }
    }

    /// Constructs an explicit policy for parser and plan-builder fixtures.
    pub const fn fixture(arch: Arch, user_top: u64) -> Self {
        Self {
            arch,
            page_size: 4096,
            user_top,
            max_phdrs: 64,
            allow_interpreter: true,
        }
    }

    pub(crate) const fn accepts_elf_flags(self, flags: u32) -> bool {
        match self.arch {
            Arch::Riscv64 => {
                const RVC: u32 = 0x0001;
                const FLOAT_ABI_MASK: u32 = 0x0006;
                const FLOAT_ABI_QUAD: u32 = 0x0006;
                const SUPPORTED: u32 = RVC | FLOAT_ABI_MASK;

                flags & !SUPPORTED == 0 && flags & FLOAT_ABI_MASK != FLOAT_ABI_QUAD
            }
            Arch::LoongArch64 => {
                const ABI_MODIFIER_MASK: u32 = 0x07;
                const ABI_DOUBLE_FLOAT: u32 = 0x03;
                const OBJABI_V1: u32 = 0x40;
                const KNOWN: u32 = ABI_MODIFIER_MASK | OBJABI_V1;

                flags & !KNOWN == 0 && flags & ABI_MODIFIER_MASK <= ABI_DOUBLE_FLOAT
            }
        }
    }
}
