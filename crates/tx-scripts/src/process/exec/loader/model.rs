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

/// Stable syntax errors returned by ELF parser adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfDecodeError {
    Truncated,
    BadMagic,
    UnsupportedClass,
    UnsupportedEndian,
    UnsupportedVersion,
    BadEntrySize,
    IntegerOverflow,
    Malformed,
}
