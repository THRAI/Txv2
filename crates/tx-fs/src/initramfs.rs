//! initramfs cpio `newc` unpacker.
//!
//! Per `bringup_fs_specs_v_1` Part II: parse a trusted cpio `newc`
//! archive from in-memory bytes and populate a tmpfs root through
//! normal VFS kernel helpers.

use alloc::vec::Vec;
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::vfs::{
    execution::{kernel_create, kernel_mkdir, kernel_symlink},
    DEntry, OpenFile, OpenFileFlags,
};

const CPIO_MAGIC: &[u8; 6] = b"070701";
const CPIO_TRAILER: &[u8; 10] = b"TRAILER!!!";

/// Header is 110 bytes; fields are 8-hex-digit ASCII with NUL terminator.
const HEADER_LEN: usize = 110;

fn parse_hex8(s: &[u8]) -> u64 {
    u64::from_str_radix(core::str::from_utf8(s).unwrap_or("0"), 16).unwrap_or(0)
}

struct CpioEntry<'a> {
    mode: u32,
    namesize: usize,
    filesize: usize,
    name: &'a [u8],
    data: &'a [u8],
}

/// Parse a single cpio entry from `data[start..]`.
/// Returns `None` on trailer or malformed header.
fn parse_entry(data: &[u8], mut offset: usize) -> Option<(CpioEntry<'_>, usize)> {
    if offset + HEADER_LEN > data.len() {
        return None;
    }

    let header = &data[offset..offset + HEADER_LEN];

    if &header[0..6] != CPIO_MAGIC {
        return None;
    }

    let namesize = parse_hex8(&header[94..102]) as usize;
    let filesize = parse_hex8(&header[54..62]) as usize;
    let mode = parse_hex8(&header[14..22]) as u32;

    offset += HEADER_LEN;

    // Read name (padded to 4-byte boundary)
    let name_end = offset + namesize;
    if name_end > data.len() {
        return None;
    }
    let name = &data[offset..offset + namesize - 1]; // strip NUL
    offset = (name_end + 3) & !3; // 4-byte align

    // Read file data (padded to 4-byte boundary)
    let data_end = offset + filesize;
    if data_end > data.len() {
        return None;
    }
    let file_data = &data[offset..data_end];
    offset = (data_end + 3) & !3;

    Some((
        CpioEntry {
            mode,
            namesize,
            filesize,
            name,
            data: file_data,
        },
        offset,
    ))
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// Unpack a cpio `newc` archive from `initrd` into the tmpfs root at `root_dentry`.
///
/// Returns the number of entries unpacked, or an error.
pub fn unpack_initramfs(
    initrd: &[u8],
    root_dentry: &Cap<DEntry>,
    mount_payload: &Cap<MountPayload>,
    guard: &Guard<'_>,
) -> Result<u64, Errno> {
    let mut offset = 0;
    let mut count = 0u64;

    loop {
        let (entry, next) = match parse_entry(initrd, offset) {
            Some(v) => v,
            None => {
                // Check if we hit the trailer
                if offset + 10 <= initrd.len()
                    && &initrd[offset..offset + 10] == CPIO_TRAILER
                {
                    break;
                }
                return Err(Errno::EINVAL);
            }
        };
        offset = next;

        // Skip "." entry
        if entry.name == b"." {
            continue;
        }

        let fmode = (entry.mode & S_IFMT) as u16;
        let perm = (entry.mode & !S_IFMT) as u16;

        if fmode == S_IFDIR as u16 {
            kernel_mkdir(root_dentry, mount_payload, entry.name, perm | 0o040000, guard)?;
        } else if fmode == S_IFREG as u16 {
            let (_dentry, open_file) =
                kernel_create(root_dentry, mount_payload, entry.name, perm | 0o100000, guard)?;
            // Write file content
            if !entry.data.is_empty() {
                let bytes = core::slice::from_ref(entry.data);
                // Use OpenFile::step_write to fill the file
                write_all(&open_file, entry.data, guard)?;
            }
        } else if fmode == S_IFLNK as u16 {
            kernel_symlink(root_dentry, mount_payload, entry.name, entry.data, guard)?;
        }
        // Skip other types (devices, fifos, sockets) for bringup

        count += 1;
    }

    Ok(count)
}

/// Write all bytes to an OpenFile via its PageContainer backing.
fn write_all(file: &Cap<OpenFile>, data: &[u8], guard: &Guard<'_>) -> Result<(), Errno> {
    use tx_subsystems::page_backed::PageContainer;
    use tx_subsystems::vfs::adapter::step_engine::{self as vfs_se, NoProgress};
    use tx_subsystems::vm::USER_PAGE_SIZE;

    if data.is_empty() {
        return Ok(());
    }

    // Get the RNode's PageContainer
    let rnode = file.rnode();
    let pc = match rnode.page_container() {
        Some(pc) => pc,
        None => return Err(Errno::EINVAL),
    };

    let page_size = USER_PAGE_SIZE as u64;
    let mut written: u64 = 0;
    let total = data.len() as u64;

    while written < total {
        let page_off = written % page_size;
        let page_idx = written / page_size;
        let chunk_end = (written + (page_size - page_off)).min(total);
        let chunk = &data[written as usize..chunk_end as usize];

        // Materialise the page (will allocate in tmpfs)
        use tx_subsystems::page_backed::MaterializeAccess;
        match pc.materialize_page(
            tx_subsystems::page_backed::PageIndex::new(page_idx),
            MaterializeAccess::Write,
            guard,
        ) {
            vfs_se::StepOutcome::Done(materialized) => {
                let ppn = materialized.ppn;
                // Write chunk into the page via the pmap
                let dst = ppn.byte_slice_mut();
                let off = page_off as usize;
                let len = chunk.len();
                dst[off..off + len].copy_from_slice(chunk);
                written += len as u64;
            }
            vfs_se::StepOutcome::Err(e) => return Err(e),
            _ => return Err(Errno::EIO),
        }
    }

    Ok(())
}
