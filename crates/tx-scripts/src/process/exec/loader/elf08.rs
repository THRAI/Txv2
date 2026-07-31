use alloc::vec::Vec;

use elf::endian::LittleEndian;
use elf::file::{self, Class, FileHeader};
use elf::parse::{ParseAt, ParseError, ParsingTable};
use elf::segment::ProgramHeader;

use super::{ElfClass, ElfDecodeError, ElfEndian, ElfFileParser, ElfHeader, ElfProgramHeader};

/// Syntax-only ELF decoder backed by `elf` 0.8's low-level parsing APIs.
pub struct Elf08Parser;

impl ElfFileParser for Elf08Parser {
    fn parse_header(bytes: &[u8]) -> Result<ElfHeader, ElfDecodeError> {
        if bytes.len() < elf::abi::EI_NIDENT {
            return Err(ElfDecodeError::Truncated);
        }

        let ident = file::parse_ident::<LittleEndian>(&bytes[..elf::abi::EI_NIDENT])
            .map_err(map_parse_error)?;
        let header_len = match ident.1 {
            Class::ELF32 => elf::abi::EI_NIDENT + file::ELF32_EHDR_TAILSIZE,
            Class::ELF64 => elf::abi::EI_NIDENT + file::ELF64_EHDR_TAILSIZE,
        };
        if bytes.len() < header_len {
            return Err(ElfDecodeError::Truncated);
        }

        let header = FileHeader::parse_tail(ident, &bytes[elf::abi::EI_NIDENT..header_len])
            .map_err(map_parse_error)?;
        Ok(ElfHeader {
            class: tx_class(header.class),
            endian: ElfEndian::Little,
            elf_type: header.e_type,
            machine: header.e_machine,
            version: header.version,
            entry: header.e_entry,
            phoff: header.e_phoff,
            shoff: header.e_shoff,
            flags: header.e_flags,
            ehsize: header.e_ehsize,
            phentsize: header.e_phentsize,
            phnum: header.e_phnum,
            shentsize: header.e_shentsize,
            shnum: header.e_shnum,
            shstrndx: header.e_shstrndx,
        })
    }

    fn parse_program_headers(
        header: &ElfHeader,
        bytes: &[u8],
    ) -> Result<Vec<ElfProgramHeader>, ElfDecodeError> {
        if header.endian != ElfEndian::Little {
            return Err(ElfDecodeError::UnsupportedEndian);
        }

        let class = elf_class(header.class);
        let entry_size = usize::from(header.phentsize);
        if entry_size != ProgramHeader::size_for(class) {
            return Err(ElfDecodeError::BadEntrySize);
        }

        let count = usize::from(header.phnum);
        let expected_len = count
            .checked_mul(entry_size)
            .ok_or(ElfDecodeError::IntegerOverflow)?;
        if bytes.len() < expected_len {
            return Err(ElfDecodeError::Truncated);
        }
        if bytes.len() != expected_len {
            return Err(ElfDecodeError::Malformed);
        }

        let table = ParsingTable::<LittleEndian, ProgramHeader>::new(LittleEndian, class, bytes);
        if table.len() != count {
            return Err(ElfDecodeError::Malformed);
        }

        let mut program_headers = Vec::new();
        program_headers
            .try_reserve_exact(count)
            .map_err(|_| ElfDecodeError::OutOfMemory)?;
        for index in 0..count {
            let header = table.get(index).map_err(map_parse_error)?;
            program_headers.push(ElfProgramHeader {
                p_type: header.p_type,
                p_flags: header.p_flags,
                p_offset: header.p_offset,
                p_vaddr: header.p_vaddr,
                p_paddr: header.p_paddr,
                p_filesz: header.p_filesz,
                p_memsz: header.p_memsz,
                p_align: header.p_align,
            });
        }
        Ok(program_headers)
    }
}

fn elf_class(class: ElfClass) -> Class {
    match class {
        ElfClass::Elf32 => Class::ELF32,
        ElfClass::Elf64 => Class::ELF64,
    }
}

fn tx_class(class: Class) -> ElfClass {
    match class {
        Class::ELF32 => ElfClass::Elf32,
        Class::ELF64 => ElfClass::Elf64,
    }
}

fn map_parse_error(error: ParseError) -> ElfDecodeError {
    match error {
        ParseError::BadMagic(_) => ElfDecodeError::BadMagic,
        ParseError::UnsupportedElfClass(_) => ElfDecodeError::UnsupportedClass,
        ParseError::UnsupportedElfEndianness(_) => ElfDecodeError::UnsupportedEndian,
        ParseError::UnsupportedVersion(_) => ElfDecodeError::UnsupportedVersion,
        ParseError::BadEntsize(_) => ElfDecodeError::BadEntrySize,
        ParseError::IntegerOverflow => ElfDecodeError::IntegerOverflow,
        ParseError::SliceReadError(_) | ParseError::TryFromSliceError(_) => {
            ElfDecodeError::Truncated
        }
        _ => ElfDecodeError::Malformed,
    }
}
