//! Bounded staged reads for an executable ELF image.

use alloc::vec::Vec;

use tx_hal::PlatformConfig;
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::{read_exact_at, PageContainer};

use super::loader::{
    build_image_plan, parse_interpreter_path, program_header_table_len, validate_header,
    Elf08Parser, ElfFileParser, ElfLoadPolicy, ExecImagePlan, ParseError, MAX_INTERP_PATH,
    PT_INTERP,
};
use crate::adapter::step_engine::{self as step_engine, StepOutcome, YieldShape};

pub use super::loader::ImageRole;

/// Stable failures produced while reading one executable image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageReadError {
    InvalidImage(ParseError),
    InvalidOffset,
    Retry,
    WouldBlock(YieldShape),
    OutOfMemory,
    Io,
}

fn map_page_backed_errno(errno: Errno) -> ImageReadError {
    match errno {
        Errno::ENOEXEC => ImageReadError::InvalidImage(ParseError::Phdr),
        Errno::EINVAL => ImageReadError::InvalidOffset,
        Errno::EAGAIN => ImageReadError::Retry,
        Errno::ENOMEM => ImageReadError::OutOfMemory,
        _ => ImageReadError::Io,
    }
}

fn try_zeroed_bytes(len: usize) -> Result<Vec<u8>, ImageReadError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| ImageReadError::OutOfMemory)?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn map_decode_error(error: super::loader::ElfDecodeError, fallback: ParseError) -> ImageReadError {
    match error {
        super::loader::ElfDecodeError::OutOfMemory => ImageReadError::OutOfMemory,
        _ => ImageReadError::InvalidImage(fallback),
    }
}

fn map_plan_error(error: ParseError) -> ImageReadError {
    match error {
        ParseError::OutOfMemory => ImageReadError::OutOfMemory,
        other => ImageReadError::InvalidImage(other),
    }
}

/// Read and validate one image from a real file-backed page container.
///
/// Materialization waits retain their `YieldShape` so the exec StepOp can park
/// on the originating wait source. No address-space state has been published
/// when this returns.
pub fn read_elf_image<P: PlatformConfig>(
    pc: &PageContainer,
    role: ImageRole,
) -> Result<ExecImagePlan, ImageReadError> {
    let guard = step_engine::guard();
    let result = read_elf_image_with::<Elf08Parser, _>(
        pc.size_bytes(),
        ElfLoadPolicy::for_platform::<P>(),
        role,
        |offset, out| match read_exact_at(pc, offset, out, &guard) {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Continue { .. } => Err(ImageReadError::Retry),
            StepOutcome::Yield { shape, .. } => Err(ImageReadError::WouldBlock(shape)),
            StepOutcome::Err(error) => {
                let errno: Errno = error.into();
                Err(map_page_backed_errno(errno))
            }
        },
    );
    drop(guard);
    result
}

pub(crate) fn read_elf_image_with<P, R>(
    file_len: u64,
    policy: ElfLoadPolicy,
    role: ImageRole,
    mut read_exact: R,
) -> Result<ExecImagePlan, ImageReadError>
where
    P: ElfFileParser,
    R: FnMut(u64, &mut [u8]) -> Result<(), ImageReadError>,
{
    let mut header_bytes = [0u8; 64];
    read_exact(0, &mut header_bytes)?;
    let header = P::parse_header(&header_bytes)
        .map_err(|error| map_decode_error(error, ParseError::Magic))?;
    validate_header(&header, policy).map_err(ImageReadError::InvalidImage)?;

    let table_len =
        program_header_table_len(&header, policy).map_err(ImageReadError::InvalidImage)?;
    header
        .phoff
        .checked_add(table_len)
        .filter(|end| *end <= file_len)
        .ok_or(ImageReadError::InvalidImage(ParseError::Phdr))?;
    let table_len =
        usize::try_from(table_len).map_err(|_| ImageReadError::InvalidImage(ParseError::Phdr))?;
    let mut table_bytes = try_zeroed_bytes(table_len)?;
    read_exact(header.phoff, &mut table_bytes)?;
    let phdrs = P::parse_program_headers(&header, &table_bytes)
        .map_err(|error| map_decode_error(error, ParseError::Phdr))?;

    let mut interp_phdr = None;
    for phdr in &phdrs {
        if phdr.p_type != PT_INTERP {
            continue;
        }
        if role == ImageRole::Interpreter || interp_phdr.replace(*phdr).is_some() {
            return Err(ImageReadError::InvalidImage(ParseError::HasInterp));
        }
    }

    let interpreter_path = if let Some(phdr) = interp_phdr {
        if phdr.p_filesz == 0 || phdr.p_filesz > MAX_INTERP_PATH {
            return Err(ImageReadError::InvalidImage(ParseError::Phdr));
        }
        phdr.p_offset
            .checked_add(phdr.p_filesz)
            .filter(|end| *end <= file_len)
            .ok_or(ImageReadError::InvalidImage(ParseError::Phdr))?;
        let len = usize::try_from(phdr.p_filesz)
            .map_err(|_| ImageReadError::InvalidImage(ParseError::Phdr))?;
        let mut bytes = try_zeroed_bytes(len)?;
        read_exact(phdr.p_offset, &mut bytes)?;
        Some(parse_interpreter_path(&bytes).map_err(ImageReadError::InvalidImage)?)
    } else {
        None
    };

    build_image_plan(&header, &phdrs, interpreter_path, file_len, policy, role)
        .map_err(map_plan_error)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use tx_hal::Arch;

    use super::{read_elf_image_with, ImageReadError, ImageRole};
    use crate::process::exec::loader::{Elf08Parser, ElfLoadPolicy, ParseError};

    struct OomParser;

    impl crate::process::exec::loader::ElfFileParser for OomParser {
        fn parse_header(
            _: &[u8],
        ) -> Result<
            crate::process::exec::loader::ElfHeader,
            crate::process::exec::loader::ElfDecodeError,
        > {
            Err(crate::process::exec::loader::ElfDecodeError::OutOfMemory)
        }

        fn parse_program_headers(
            _: &crate::process::exec::loader::ElfHeader,
            _: &[u8],
        ) -> Result<
            Vec<crate::process::exec::loader::ElfProgramHeader>,
            crate::process::exec::loader::ElfDecodeError,
        > {
            Err(crate::process::exec::loader::ElfDecodeError::OutOfMemory)
        }
    }

    const ELF64_EHDR_SIZE: usize = 64;
    const ELF64_PHENT_SIZE: usize = 56;
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    const PT_PHDR: u32 = 6;
    const PT_GNU_STACK: u32 = 0x6474_e551;
    const PT_RISCV_ATTRIBUTES: u32 = 0x7000_0003;
    const PF_R: u32 = 4;
    const PF_W: u32 = 2;
    const PF_X: u32 = 1;
    const LOAD_VADDR: u64 = 0x1_0000;

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[allow(clippy::too_many_arguments)]
    fn write_phdr(
        bytes: &mut [u8],
        offset: usize,
        p_type: u32,
        flags: u32,
        file_offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
        align: u64,
    ) {
        write_u32(bytes, offset, p_type);
        write_u32(bytes, offset + 4, flags);
        write_u64(bytes, offset + 8, file_offset);
        write_u64(bytes, offset + 16, vaddr);
        write_u64(bytes, offset + 24, vaddr);
        write_u64(bytes, offset + 32, filesz);
        write_u64(bytes, offset + 40, memsz);
        write_u64(bytes, offset + 48, align);
    }

    fn staged_fixture(phoff: u64, interp: Option<(u64, &[u8])>) -> Vec<u8> {
        let phnum = if interp.is_some() { 3u16 } else { 2u16 };
        let table_len = u64::from(phnum) * ELF64_PHENT_SIZE as u64;
        let table_end = phoff + table_len;
        let file_len = interp
            .map(|(offset, path)| offset + path.len() as u64)
            .unwrap_or(table_end)
            .max(table_end);
        let mut bytes = vec![0u8; file_len as usize];

        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        write_u16(&mut bytes, 16, 2);
        write_u16(&mut bytes, 18, 243);
        write_u32(&mut bytes, 20, 1);
        write_u64(&mut bytes, 24, LOAD_VADDR + 0x80);
        write_u64(&mut bytes, 32, phoff);
        write_u16(&mut bytes, 52, ELF64_EHDR_SIZE as u16);
        write_u16(&mut bytes, 54, ELF64_PHENT_SIZE as u16);
        write_u16(&mut bytes, 56, phnum);

        let table = phoff as usize;
        write_phdr(
            &mut bytes,
            table,
            PT_PHDR,
            PF_R,
            phoff,
            LOAD_VADDR + phoff,
            table_len,
            table_len,
            8,
        );
        write_phdr(
            &mut bytes,
            table + ELF64_PHENT_SIZE,
            PT_LOAD,
            PF_R | PF_X,
            0,
            LOAD_VADDR,
            file_len,
            file_len,
            4096,
        );
        if let Some((offset, path)) = interp {
            write_phdr(
                &mut bytes,
                table + ELF64_PHENT_SIZE * 2,
                PT_INTERP,
                PF_R,
                offset,
                0,
                path.len() as u64,
                path.len() as u64,
                1,
            );
            let start = offset as usize;
            bytes[start..start + path.len()].copy_from_slice(path);
        }
        bytes
    }

    fn read_fixture(
        bytes: &[u8],
        role: ImageRole,
    ) -> Result<crate::process::exec::loader::ExecImagePlan, ImageReadError> {
        read_elf_image_with::<Elf08Parser, _>(
            bytes.len() as u64,
            ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
            role,
            |offset, out| {
                let start = usize::try_from(offset).map_err(|_| ImageReadError::InvalidOffset)?;
                let end = start
                    .checked_add(out.len())
                    .ok_or(ImageReadError::InvalidOffset)?;
                let source = bytes
                    .get(start..end)
                    .ok_or(ImageReadError::InvalidImage(ParseError::Phdr))?;
                out.copy_from_slice(source);
                Ok(())
            },
        )
    }

    #[test]
    fn staged_elf_read_preserves_injected_io_for_main_and_interpreter() {
        for role in [ImageRole::Main, ImageRole::Interpreter] {
            let result = read_elf_image_with::<Elf08Parser, _>(
                64,
                ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
                role,
                |_, _| Err(ImageReadError::Io),
            );
            assert_eq!(result, Err(ImageReadError::Io));
        }
    }

    #[test]
    fn staged_elf_read_preserves_decoder_out_of_memory() {
        let result = read_elf_image_with::<OomParser, _>(
            64,
            ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
            ImageRole::Main,
            |_, out| {
                out.fill(0);
                Ok(())
            },
        );
        assert_eq!(result, Err(ImageReadError::OutOfMemory));
    }

    #[test]
    fn staged_zeroed_buffer_reports_capacity_as_out_of_memory() {
        assert_eq!(
            super::try_zeroed_bytes(usize::MAX),
            Err(ImageReadError::OutOfMemory)
        );
    }

    #[test]
    fn staged_elf_read_accepts_static_riscv_toolchain_header_shape() {
        // Mirrors the freestanding vDSO guest witness: an ET_EXEC image with
        // one RX LOAD, a RISC-V attributes header, and a non-executable
        // GNU-stack declaration, but no PT_PHDR record.
        let file_len = 0x102eusize;
        let mut bytes = vec![0u8; file_len];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        write_u16(&mut bytes, 16, 2);
        write_u16(&mut bytes, 18, 243);
        write_u32(&mut bytes, 20, 1);
        write_u64(&mut bytes, 24, 0x10bcc);
        write_u64(&mut bytes, 32, 64);
        write_u16(&mut bytes, 52, ELF64_EHDR_SIZE as u16);
        write_u16(&mut bytes, 54, ELF64_PHENT_SIZE as u16);
        write_u16(&mut bytes, 56, 3);

        write_phdr(
            &mut bytes,
            64,
            PT_RISCV_ATTRIBUTES,
            PF_R,
            0xfba,
            0,
            0x74,
            0,
            1,
        );
        write_phdr(
            &mut bytes,
            64 + ELF64_PHENT_SIZE,
            PT_LOAD,
            PF_R | PF_X,
            0,
            LOAD_VADDR,
            0xfa1,
            0xfa1,
            4096,
        );
        write_phdr(
            &mut bytes,
            64 + ELF64_PHENT_SIZE * 2,
            PT_GNU_STACK,
            PF_R | PF_W,
            0,
            0,
            0,
            0,
            16,
        );

        let plan = read_fixture(&bytes, ImageRole::Main).expect("static toolchain image parses");
        assert_eq!(plan.entry, 0x10bcc);
        assert_eq!(plan.at_phdr, LOAD_VADDR + 64);
        assert!(!plan.stack.executable_requested);
    }

    #[test]
    fn staged_elf_read_accepts_phdr_table_at_8192() {
        let bytes = staged_fixture(8192, None);
        let plan = read_fixture(&bytes, ImageRole::Main).expect("staged phdr read");
        assert_eq!(plan.at_phnum, 2);
        assert_eq!(plan.at_phdr, LOAD_VADDR + 8192);
    }

    #[test]
    fn staged_elf_read_fetches_interp_beyond_first_page() {
        let path = b"/lib/ld-musl-riscv64.so.1\0";
        let bytes = staged_fixture(8192, Some((12_288, path)));
        let plan = read_fixture(&bytes, ImageRole::Main).expect("staged PT_INTERP read");
        assert_eq!(
            plan.interpreter_path.as_deref(),
            Some(&path[..path.len() - 1])
        );
        assert!(matches!(
            read_fixture(&bytes, ImageRole::Interpreter),
            Err(ImageReadError::InvalidImage(ParseError::HasInterp))
        ));
    }

    #[test]
    fn staged_elf_read_rejects_relative_interp_path() {
        let bytes = staged_fixture(8192, Some((12_288, b"ld.so\0")));
        assert!(matches!(
            read_fixture(&bytes, ImageRole::Main),
            Err(ImageReadError::InvalidImage(ParseError::Phdr))
        ));
    }

    #[test]
    fn staged_elf_read_rejects_extreme_phoff_without_reading_it() {
        let mut bytes = staged_fixture(64, None);
        bytes.truncate(ELF64_EHDR_SIZE);
        write_u64(&mut bytes, 32, u64::MAX - 16);
        let mut requests = 0usize;
        let result = read_elf_image_with::<Elf08Parser, _>(
            bytes.len() as u64,
            ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
            ImageRole::Main,
            |offset, out| {
                requests += 1;
                let start = usize::try_from(offset).map_err(|_| ImageReadError::InvalidOffset)?;
                let end = start
                    .checked_add(out.len())
                    .ok_or(ImageReadError::InvalidOffset)?;
                let source = bytes
                    .get(start..end)
                    .ok_or(ImageReadError::InvalidImage(ParseError::Phdr))?;
                out.copy_from_slice(source);
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(ImageReadError::InvalidImage(ParseError::Phdr))
        ));
        assert_eq!(requests, 1, "only the 64-byte ELF header may be read");
    }

    #[test]
    fn staged_elf_read_rejects_extreme_interp_offset_without_reading_it() {
        let path = b"/lib/ld.so\0";
        let mut bytes = staged_fixture(8192, Some((12_288, path)));
        let interp_phdr = 8192 + ELF64_PHENT_SIZE * 2;
        write_u64(&mut bytes, interp_phdr + 8, u64::MAX - 4);
        let mut requests = 0usize;
        let result = read_elf_image_with::<Elf08Parser, _>(
            bytes.len() as u64,
            ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
            ImageRole::Main,
            |offset, out| {
                requests += 1;
                let start = usize::try_from(offset).map_err(|_| ImageReadError::InvalidOffset)?;
                let end = start
                    .checked_add(out.len())
                    .ok_or(ImageReadError::InvalidOffset)?;
                let source = bytes
                    .get(start..end)
                    .ok_or(ImageReadError::InvalidImage(ParseError::Phdr))?;
                out.copy_from_slice(source);
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(ImageReadError::InvalidImage(ParseError::Phdr))
        ));
        assert_eq!(
            requests, 2,
            "the invalid interpreter range must not be read"
        );
    }

    #[test]
    fn staged_elf_read_propagates_out_of_memory_from_reader() {
        let result = read_elf_image_with::<Elf08Parser, _>(
            ELF64_EHDR_SIZE as u64,
            ElfLoadPolicy::fixture(Arch::Riscv64, 0x4000_0000),
            ImageRole::Main,
            |_offset, _out| Err(ImageReadError::OutOfMemory),
        );

        assert_eq!(result, Err(ImageReadError::OutOfMemory));
    }

    #[test]
    fn page_backed_enomem_maps_to_image_read_out_of_memory() {
        assert_eq!(
            super::map_page_backed_errno(tx_subsystems::execution::Errno::ENOMEM),
            ImageReadError::OutOfMemory
        );
    }
}
