//! Newc cpio (070701) initramfs unpacker.
//!
//! Walks a `&[u8]` cpio archive and reproduces its file tree inside a
//! target tmpfs mount via the production `FsOps` surface. Mirrors
//! `tx_kernel::init::CoreInit::register_init_fixture_into_tmpfs`'s
//! create_inode → materialise_rnode → page-by-page memcpy → truncate
//! shape, generalised to multiple files + directories + symlinks.
//!
//! Format: newc cpio (`man 5 cpio`):
//!   - 110-byte ASCII-hex header per entry, magic `070701` (newc) or
//!     `070702` (newc with CRC; CRC is treated as advisory and skipped).
//!   - Filename (NUL-terminated, padded so header+name length is a
//!     multiple of 4).
//!   - File data (padded to 4-byte alignment).
//!   - Archive ends with an entry named `TRAILER!!!`.
//!
//! Supported entry kinds (via S_IFMT bits in c_mode):
//!   - S_IFREG (regular files): create_inode + page-by-page memcpy.
//!   - S_IFDIR (directories): mkdir.
//!   - S_IFLNK (symlinks): symlink with the file data as target.
//!   - S_IFCHR / S_IFBLK / S_IFIFO / S_IFSOCK: skip with `unsupported`
//!     stat bump (no panic).
//!
//! Hardlink coalescing (multiple entries with same `c_ino`) is NOT
//! supported in v1; each entry is treated as an independent inode.
//! `TRAILER!!!` marks end-of-archive.

use alloc::sync::Arc;

use crate::execution::Errno;
use crate::mount::MountIdentity;
use crate::page_backed::{FsPageBacking, MaterializeAccess, PageIndex};
use crate::vfs::{
    Credential, FsObjectId, FsOps, RNodeBacking, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
use tx_substrate::step_v3::StepOutcome as V3;
use tx_substrate::zone::Cap;

#[cfg(test)]
mod tests;

/// 110-byte newc-cpio header (excluding the filename trailer).
const NEWC_HEADER_LEN: usize = 110;

/// Newc magic bytes ("070701", regular ASCII; "070702" carries an
/// advisory CRC field but the layout is otherwise identical).
const NEWC_MAGIC: &[u8] = b"070701";
const NEWC_CRC_MAGIC: &[u8] = b"070702";

/// Archive trailer name.
const TRAILER: &[u8] = b"TRAILER!!!";

/// Page size used for the memcpy loop. Mirrors
/// `tx_subsystems::vm::USER_PAGE_SIZE` so the tx-kernel boot path's
/// `register_init_fixture_into_tmpfs` and the unpacker tile pages
/// identically. We re-export the constant rather than reaching into
/// `vm` to keep the module compile-only on the FsOps surface.
const USER_PAGE_SIZE: usize = crate::vm::USER_PAGE_SIZE;

/// Reader that yields `CpioEntry` borrows from a `&[u8]` newc archive.
///
/// The reader does not allocate; entry names and data are sub-slices
/// of the source buffer. Stops when it sees the `TRAILER!!!` entry
/// (returns `None` from then on).
pub struct CpioReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
    done: bool,
}

impl<'a> CpioReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            done: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CpioEntry<'a> {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub name: &'a [u8],
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpioError {
    /// Archive truncated mid-header or mid-data.
    Truncated,
    /// Header magic does not match `070701` / `070702`.
    BadMagic,
    /// Header field could not be parsed as 8-byte ASCII hex.
    BadHeader,
}

impl<'a> Iterator for CpioReader<'a> {
    type Item = Result<CpioEntry<'a>, CpioError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match parse_one_entry(self.bytes, self.cursor) {
            Ok(Some((entry, next_cursor))) => {
                self.cursor = next_cursor;
                if entry.name == TRAILER {
                    self.done = true;
                    return None;
                }
                Some(Ok(entry))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

fn parse_one_entry(
    bytes: &[u8],
    cursor: usize,
) -> Result<Option<(CpioEntry<'_>, usize)>, CpioError> {
    if cursor >= bytes.len() {
        return Ok(None);
    }
    if cursor
        .checked_add(NEWC_HEADER_LEN)
        .is_none_or(|end| end > bytes.len())
    {
        return Err(CpioError::Truncated);
    }
    let header = &bytes[cursor..cursor + NEWC_HEADER_LEN];
    let magic = &header[0..6];
    if magic != NEWC_MAGIC && magic != NEWC_CRC_MAGIC {
        return Err(CpioError::BadMagic);
    }
    // newc layout (each field 8 ASCII-hex chars, big-endian):
    //   c_magic[6], c_ino[8], c_mode[8], c_uid[8], c_gid[8],
    //   c_nlink[8], c_mtime[8], c_filesize[8], c_devmajor[8],
    //   c_devminor[8], c_rdevmajor[8], c_rdevminor[8],
    //   c_namesize[8], c_check[8].
    // 6 + 13*8 = 110 bytes total before the name.
    let mode = ascii_hex8_to_u32(&header[14..22])?;
    let uid = ascii_hex8_to_u32(&header[22..30])?;
    let gid = ascii_hex8_to_u32(&header[30..38])?;
    let mtime = ascii_hex8_to_u32(&header[46..54])?;
    let filesize = ascii_hex8_to_u32(&header[54..62])? as usize;
    let namesize = ascii_hex8_to_u32(&header[94..102])? as usize;
    if namesize == 0 {
        return Err(CpioError::BadHeader);
    }

    // Name follows the 110-byte header; the (header+name) total is
    // padded up to a 4-byte boundary. The trailing NUL is included
    // in `namesize`.
    let name_start = cursor
        .checked_add(NEWC_HEADER_LEN)
        .ok_or(CpioError::Truncated)?;
    let name_end = name_start
        .checked_add(namesize)
        .ok_or(CpioError::Truncated)?;
    if name_end > bytes.len() {
        return Err(CpioError::Truncated);
    }
    let name_with_nul = &bytes[name_start..name_end];
    let name = trim_trailing_nuls(name_with_nul);
    let after_name = name_end;
    let after_name_aligned = align_up_4(after_name).map_err(|_| CpioError::Truncated)?;

    // Data follows the aligned name region. File data is
    // 4-byte-aligned both at start (because header+name is padded) and
    // at end.
    let data_start = after_name_aligned;
    let data_end = data_start
        .checked_add(filesize)
        .ok_or(CpioError::Truncated)?;
    if data_end > bytes.len() {
        return Err(CpioError::Truncated);
    }
    let data = &bytes[data_start..data_end];
    let next_cursor = align_up_4(data_end).map_err(|_| CpioError::Truncated)?;

    Ok(Some((
        CpioEntry {
            mode,
            uid,
            gid,
            mtime,
            name,
            data,
        },
        next_cursor,
    )))
}

fn ascii_hex8_to_u32(bytes: &[u8]) -> Result<u32, CpioError> {
    if bytes.len() != 8 {
        return Err(CpioError::BadHeader);
    }
    let mut acc: u32 = 0;
    for &b in bytes {
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(CpioError::BadHeader),
        };
        acc = (acc << 4) | (nibble as u32);
    }
    Ok(acc)
}

fn align_up_4(n: usize) -> Result<usize, ()> {
    n.checked_add(3).map(|v| v & !3usize).ok_or(())
}

fn trim_trailing_nuls(bytes: &[u8]) -> &[u8] {
    let mut len = bytes.len();
    while len > 0 && bytes[len - 1] == 0 {
        len -= 1;
    }
    &bytes[..len]
}

/// Aggregate counters returned by `unpack_into_root_mount`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnpackStats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes_total: u64,
    pub unsupported: usize,
}

/// Errors surfaced by `unpack_into_root_mount`.
#[derive(Clone, Copy, Debug)]
pub enum UnpackError {
    /// Cpio parser failure.
    Parse(CpioError),
    /// FsOps step returned an `Errno`. Carries the failed operation
    /// label (for diagnostics) and the errno.
    FsOp { op: &'static str, errno: Errno },
    /// Got an `Advanced(_)` result from a step body that the unpacker
    /// can't service synchronously. Should not occur against a
    /// freshly-mounted tmpfs.
    UnexpectedAdvance(&'static str),
}

impl From<CpioError> for UnpackError {
    fn from(e: CpioError) -> Self {
        UnpackError::Parse(e)
    }
}

/// Unpack the cpio archive at `bytes` into the rootfs identified by
/// `root_mount`. Mirrors `register_init_fixture_into_tmpfs`'s control
/// flow: for every entry, walk-or-create parent directories, then
/// dispatch on the entry's S_IFMT bits to either `create_inode` +
/// page memcpy + truncate (regular file), `mkdir` (directory), or
/// `symlink` (symlink).
///
/// Idempotent on existing entries: a regular-file entry whose name
/// already exists in the parent returns `EEXIST` from `create_inode`,
/// which the unpacker treats as a non-fatal skip (the prior file's
/// contents win). Directories with `EEXIST` are also tolerated — many
/// cpio writers emit `bin/` after `bin/foo`, and the unpacker has
/// already had to materialise `bin` to create `foo`.
pub fn unpack_into_root_mount(
    bytes: &[u8],
    root_mount: &Cap<MountIdentity>,
) -> Result<UnpackStats, UnpackError> {
    // Mount-identity payload is upgraded via `payload_cap()` on the
    // post-merge API; `into_cap()` recovers a `Cap<MountPayload>`
    // that the unpacker's downstream code expects.
    let payload = root_mount
        .payload_cap()
        .map_err(|_| UnpackError::FsOp {
            op: "payload_cap",
            errno: Errno::EIO,
        })?
        .into_cap();
    // Errnos from the trait surfaces route through
    // `Errno::from(step_v3::Errno)` into the existing
    // `UnpackError::FsOp { errno: Errno, .. }` carrier.
    let fs_ops = payload.fs_ops.clone();
    let fs_page_backing = payload.fs_page_backing.clone();
    let root_object_id = root_mount.root().fs_object_id();

    let mut stats = UnpackStats::default();
    let cred = Credential::root();

    let reader = CpioReader::new(bytes);
    for entry in reader {
        let entry = entry?;
        // Per `man 5 cpio`, the trailer record is `TRAILER!!!` and
        // also acts as end-of-archive; the iterator already filters
        // it out, so we never see it here.

        // Skip empty / "." entries some cpio writers emit.
        if entry.name.is_empty() || entry.name == b"." {
            continue;
        }

        let kind_bits = (entry.mode as u16) & S_IFMT;
        let mode_low = (entry.mode as u16) & !S_IFMT;

        // Split the path; `filename` is the last component, `parents`
        // are the leading components.
        let (parents, filename) = split_path(entry.name);
        if filename.is_empty() {
            // Trailing slash — directory-only entry like "bin/".
            // Walk-or-create the parents themselves.
            walk_or_create_dirs(&fs_ops, root_object_id, parents, &cred)?;
            stats.dirs += 1;
            continue;
        }
        let parent_id = walk_or_create_dirs(&fs_ops, root_object_id, parents, &cred)?;

        match kind_bits {
            S_IFREG => {
                unpack_regular(
                    &fs_ops,
                    &fs_page_backing,
                    parent_id,
                    filename,
                    mode_low,
                    entry.data,
                    &cred,
                )?;
                stats.files += 1;
                stats.bytes_total = stats.bytes_total.saturating_add(entry.data.len() as u64);
            }
            S_IFDIR => {
                mkdir_idempotent(&fs_ops, parent_id, filename, mode_low, &cred)?;
                stats.dirs += 1;
            }
            S_IFLNK => {
                unpack_symlink(&fs_ops, parent_id, filename, entry.data, &cred)?;
                stats.symlinks += 1;
            }
            _ => {
                // Unsupported special files (char/block/fifo/socket).
                // Skip to keep the boot path warn-and-continue rather
                // than wedge.
                stats.unsupported += 1;
            }
        }
    }
    Ok(stats)
}

fn split_path(path: &[u8]) -> (&[u8], &[u8]) {
    // Strip a single leading `/` so absolute and relative cpio paths
    // both anchor at the mount root. cpio archives produced by `find
    // | cpio -o -H newc` typically strip the leading `/`, but some
    // archives keep it.
    let path = if path.first() == Some(&b'/') {
        &path[1..]
    } else {
        path
    };
    match path.iter().rposition(|&b| b == b'/') {
        Some(idx) => (&path[..idx], &path[idx + 1..]),
        None => (&[], path),
    }
}

fn walk_or_create_dirs(
    fs_ops: &Arc<dyn FsOps>,
    root_object_id: FsObjectId,
    path: &[u8],
    cred: &Credential,
) -> Result<FsObjectId, UnpackError> {
    let mut current = root_object_id;
    for component in path.split(|&b| b == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        // Try lookup first; mkdir if missing.
        let guard = tx_substrate::epoch::guard();
        match fs_ops.lookup(current, component, &guard) {
            V3::Done(id) => {
                current = id;
            }
            V3::Err(v3_errno) => {
                let errno = Errno::from(v3_errno);
                if errno == Errno::ENOENT {
                    drop(guard);
                    let id = mkdir_idempotent(fs_ops, current, component, 0o755, cred)?;
                    current = id;
                } else {
                    return Err(UnpackError::FsOp {
                        op: "lookup",
                        errno,
                    });
                }
            }
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(UnpackError::UnexpectedAdvance("lookup"));
            }
        }
    }
    Ok(current)
}

fn mkdir_idempotent(
    fs_ops: &Arc<dyn FsOps>,
    parent: FsObjectId,
    name: &[u8],
    mode_low: u16,
    cred: &Credential,
) -> Result<FsObjectId, UnpackError> {
    let guard = tx_substrate::epoch::guard();
    match fs_ops.mkdir(parent, name, mode_low, cred, &guard) {
        V3::Done((id, _)) => Ok(id),
        V3::Err(v3_errno) => {
            let errno = Errno::from(v3_errno);
            if errno == Errno::EEXIST {
                // Directory already exists — look it up and return the id.
                match fs_ops.lookup(parent, name, &guard) {
                    V3::Done(id) => Ok(id),
                    V3::Err(v3_errno) => Err(UnpackError::FsOp {
                        op: "mkdir-eexist-relookup",
                        errno: Errno::from(v3_errno),
                    }),
                    V3::Continue { .. } | V3::Yield { .. } => {
                        Err(UnpackError::UnexpectedAdvance("mkdir-eexist-relookup"))
                    }
                }
            } else {
                Err(UnpackError::FsOp { op: "mkdir", errno })
            }
        }
        V3::Continue { .. } | V3::Yield { .. } => Err(UnpackError::UnexpectedAdvance("mkdir")),
    }
}

fn unpack_regular(
    fs_ops: &Arc<dyn FsOps>,
    fs_page_backing: &Arc<dyn FsPageBacking>,
    parent_id: FsObjectId,
    name: &[u8],
    mode_low: u16,
    data: &[u8],
    cred: &Credential,
) -> Result<(), UnpackError> {
    // Allocate the inode. mode_low carries the rwx bits; we do not
    // re-OR S_IFREG since `Tmpfs::create_inode` does that internally.
    let (file_id, file_meta) = {
        let guard = tx_substrate::epoch::guard();
        match fs_ops.create_inode(parent_id, name, mode_low, cred, &guard) {
            V3::Done(out) => out,
            V3::Err(v3_errno) => {
                let errno = Errno::from(v3_errno);
                if errno == Errno::EEXIST {
                    // Pre-existing file under that name — leave it alone.
                    return Ok(());
                }
                return Err(UnpackError::FsOp {
                    op: "create_inode",
                    errno,
                });
            }
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(UnpackError::UnexpectedAdvance("create_inode"));
            }
        }
    };

    // Materialise the inode's RNode and grab its
    // `Cap<PageContainer>` so we can populate pages via the same
    // direct-map path `register_init_fixture_into_tmpfs` uses.
    let pc = {
        let guard = tx_substrate::epoch::guard();
        let outcome = fs_ops.materialise_rnode(file_id, file_meta, &guard);
        let rnode = match outcome {
            V3::Done(r) => r,
            V3::Err(v3_errno) => {
                return Err(UnpackError::FsOp {
                    op: "materialise_rnode",
                    errno: Errno::from(v3_errno),
                });
            }
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(UnpackError::UnexpectedAdvance("materialise_rnode"));
            }
        };
        match rnode.backing() {
            RNodeBacking::PageBacked { pc } => pc.clone(),
            _other => {
                // Non-PageBacked backing for a regular file: surface
                // as ENOSYS for the operation and let the caller
                // panic-or-warn at its discretion.
                return Err(UnpackError::FsOp {
                    op: "materialise_rnode-non-pagebacked",
                    errno: Errno::ENOSYS,
                });
            }
        }
    };

    // Page-by-page memcpy via the kernel direct map. Handles the
    // empty-data case naturally (no chunks → loop is a no-op).
    for (idx, chunk) in data.chunks(USER_PAGE_SIZE).enumerate() {
        let materialised = pc
            .materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write)
            .map_err(|_| UnpackError::FsOp {
                op: "materialize_anon",
                errno: Errno::ENOMEM,
            })?;
        let frame_base = tx_substrate::page_allocator::frame_kernel_addr(materialised.ppn)
            .map_err(|_| UnpackError::FsOp {
                op: "frame_kernel_addr",
                errno: Errno::EFAULT,
            })?;
        // SAFETY: `materialised.map_pin` keeps the page resident for
        // this scope; destination region covers exactly `chunk.len()`
        // bytes from a freshly materialised anon frame; source and
        // destination do not overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
        }
    }

    // Set the visible byte size.
    let size = data.len() as u64;
    {
        let guard = tx_substrate::epoch::guard();
        match fs_page_backing.truncate(file_id, size, &guard) {
            V3::Done(()) => {}
            V3::Err(v3_errno) => {
                return Err(UnpackError::FsOp {
                    op: "truncate",
                    errno: Errno::from(v3_errno),
                });
            }
            V3::Continue { .. } | V3::Yield { .. } => {
                return Err(UnpackError::UnexpectedAdvance("truncate"));
            }
        }
    }
    Ok(())
}

fn unpack_symlink(
    fs_ops: &Arc<dyn FsOps>,
    parent_id: FsObjectId,
    name: &[u8],
    target: &[u8],
    cred: &Credential,
) -> Result<(), UnpackError> {
    let guard = tx_substrate::epoch::guard();
    match fs_ops.symlink(parent_id, name, target, cred, &guard) {
        V3::Done(_) => Ok(()),
        V3::Err(v3_errno) => {
            let errno = Errno::from(v3_errno);
            if errno == Errno::EEXIST {
                Ok(())
            } else {
                Err(UnpackError::FsOp {
                    op: "symlink",
                    errno,
                })
            }
        }
        V3::Continue { .. } | V3::Yield { .. } => Err(UnpackError::UnexpectedAdvance("symlink")),
    }
}
