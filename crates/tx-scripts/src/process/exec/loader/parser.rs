use alloc::vec::Vec;

use super::{ElfDecodeError, ElfHeader, ElfProgramHeader};

/// Syntax-only ELF decoder boundary implemented by replaceable parser adapters.
pub trait ElfFileParser {
    fn parse_header(bytes: &[u8]) -> Result<ElfHeader, ElfDecodeError>;

    fn parse_program_headers(
        header: &ElfHeader,
        bytes: &[u8],
    ) -> Result<Vec<ElfProgramHeader>, ElfDecodeError>;
}
