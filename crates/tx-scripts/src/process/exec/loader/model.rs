/// ELF object class decoded from `e_ident[EI_CLASS]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfClass {
    Elf32,
    Elf64,
}

/// ELF byte order decoded from `e_ident[EI_DATA]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfEndian {
    Little,
    Big,
}

/// Parser-independent ELF file-header fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfHeader {
    pub class: ElfClass,
    pub endian: ElfEndian,
    pub elf_type: u16,
    pub machine: u16,
    pub version: u32,
    pub entry: u64,
    pub phoff: u64,
    pub shoff: u64,
    pub flags: u32,
    pub ehsize: u16,
    pub phentsize: u16,
    pub phnum: u16,
    pub shentsize: u16,
    pub shnum: u16,
    pub shstrndx: u16,
}

impl ElfHeader {
    /// Builds the ELF64 little-endian header shape used by tests and adapters.
    pub const fn elf64_le(
        elf_type: u16,
        machine: u16,
        entry: u64,
        phoff: u64,
        phentsize: u16,
        phnum: u16,
        flags: u32,
    ) -> Self {
        Self {
            class: ElfClass::Elf64,
            endian: ElfEndian::Little,
            elf_type,
            machine,
            version: 1,
            entry,
            phoff,
            shoff: 0,
            flags,
            ehsize: 64,
            phentsize,
            phnum,
            shentsize: 0,
            shnum: 0,
            shstrndx: 0,
        }
    }
}

/// Parser-independent ELF program-header fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// A validated range in the final in-memory image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageRange {
    /// Virtual address after applying the image load bias.
    pub vaddr: u64,
    pub size: u64,
}

/// Initial-exec TLS bytes described by `PT_TLS`.
///
/// The kernel records this template for libc or the userspace interpreter; it
/// does not allocate a thread pointer or interpret a TLS ABI itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlsTemplate {
    /// Virtual address after applying the image load bias.
    pub vaddr: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub align: u64,
}

/// Executable-stack request derived from `PT_GNU_STACK`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StackRequest {
    /// False when `PT_GNU_STACK` is absent or does not carry `PF_X`.
    pub executable_requested: bool,
}

/// Stable syntax errors returned by ELF parser adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfDecodeError {
    OutOfMemory,
    Truncated,
    BadMagic,
    UnsupportedClass,
    UnsupportedEndian,
    UnsupportedVersion,
    BadEntrySize,
    IntegerOverflow,
    Malformed,
}

/// Failures while rebasing or composing final userspace ELF ranges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfLayoutError {
    OutOfMemory,
    AddressOverflow,
    InvalidAlignment,
    UserRange,
    Overlap,
    Exhausted,
}
