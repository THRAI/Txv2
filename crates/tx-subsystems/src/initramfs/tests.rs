//! Cpio-reader unit tests.
//!
//! Integration tests against `unpack_into_root_mount` (which needs a
//! real `Tmpfs` mount) live in `crates/tx-fs/src/initramfs/tests.rs`
//! per the standard separation between parser-only checks and
//! end-to-end FsOps drive paths.

use alloc::vec;
use alloc::vec::Vec;

use super::{CpioError, CpioReader, NEWC_HEADER_LEN};

/// Build a tiny newc-cpio archive holding the supplied entries plus
/// the canonical `TRAILER!!!` marker.
///
/// `entries` is a slice of `(name_bytes, mode, data_bytes)` triples;
/// each entry uses ino=1, uid=0, gid=0, mtime=0, nlinks=1, no devs.
fn build_test_cpio(entries: &[(&[u8], u32, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut next_ino: u32 = 1;
    for (name, mode, data) in entries {
        emit_entry(&mut buf, *name, *mode, data, next_ino);
        next_ino = next_ino.wrapping_add(1);
    }
    // Trailer with 0 filesize and the magic name.
    emit_entry(&mut buf, b"TRAILER!!!", 0, &[], 0);
    buf
}

fn emit_entry(buf: &mut Vec<u8>, name: &[u8], mode: u32, data: &[u8], ino: u32) {
    let namesize = name.len() + 1; // include trailing NUL
    let header_start = buf.len();
    buf.extend_from_slice(b"070701"); // c_magic
    buf.extend_from_slice(&hex8(ino));
    buf.extend_from_slice(&hex8(mode));
    buf.extend_from_slice(&hex8(0)); // uid
    buf.extend_from_slice(&hex8(0)); // gid
    buf.extend_from_slice(&hex8(1)); // nlink
    buf.extend_from_slice(&hex8(0)); // mtime
    buf.extend_from_slice(&hex8(data.len() as u32)); // filesize
    buf.extend_from_slice(&hex8(0)); // devmajor
    buf.extend_from_slice(&hex8(0)); // devminor
    buf.extend_from_slice(&hex8(0)); // rdevmajor
    buf.extend_from_slice(&hex8(0)); // rdevminor
    buf.extend_from_slice(&hex8(namesize as u32));
    buf.extend_from_slice(&hex8(0)); // check (unused for newc)
    debug_assert_eq!(buf.len() - header_start, NEWC_HEADER_LEN);
    buf.extend_from_slice(name);
    buf.push(0);
    while (buf.len() & 3) != 0 {
        buf.push(0);
    }
    buf.extend_from_slice(data);
    while (buf.len() & 3) != 0 {
        buf.push(0);
    }
}

fn hex8(value: u32) -> [u8; 8] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = [0u8; 8];
    for i in 0..8 {
        let shift = (7 - i) * 4;
        out[i] = HEX[((value >> shift) & 0xf) as usize];
    }
    out
}

#[test]
fn cpio_reader_walks_simple_archive() {
    let archive = build_test_cpio(&[(b"hello", 0o100644, b"world\n")]);
    let mut reader = CpioReader::new(&archive);

    let entry = reader.next().expect("entry").expect("ok");
    assert_eq!(entry.name, b"hello");
    assert_eq!(entry.mode & 0o170000, 0o100000);
    assert_eq!(entry.mode & 0o7777, 0o644);
    assert_eq!(entry.data, b"world\n");

    assert!(reader.next().is_none(), "trailer should end iteration");
}

#[test]
fn cpio_reader_handles_directory_entry() {
    let archive = build_test_cpio(&[(b"dir", 0o040755, b"")]);
    let mut reader = CpioReader::new(&archive);

    let entry = reader.next().expect("entry").expect("ok");
    assert_eq!(entry.name, b"dir");
    assert_eq!(entry.mode & 0o170000, 0o040000);
    assert_eq!(entry.data.len(), 0);

    assert!(reader.next().is_none());
}

#[test]
fn cpio_reader_handles_symlink_entry() {
    let archive = build_test_cpio(&[(b"link", 0o120777, b"target")]);
    let mut reader = CpioReader::new(&archive);

    let entry = reader.next().expect("entry").expect("ok");
    assert_eq!(entry.name, b"link");
    assert_eq!(entry.mode & 0o170000, 0o120000);
    assert_eq!(entry.data, b"target");

    assert!(reader.next().is_none());
}

#[test]
fn cpio_reader_walks_multiple_entries() {
    let archive = build_test_cpio(&[
        (b"a", 0o100644, b"AA"),
        (b"bin", 0o040755, b""),
        (b"bin/sh", 0o100755, b"shellbytes"),
    ]);
    let mut reader = CpioReader::new(&archive);

    let names: Vec<Vec<u8>> = (&mut reader)
        .map(|e| e.expect("ok").name.to_vec())
        .collect();
    assert_eq!(names.len(), 3);
    assert_eq!(names[0], b"a");
    assert_eq!(names[1], b"bin");
    assert_eq!(names[2], b"bin/sh");
}

#[test]
fn cpio_reader_stops_at_trailer() {
    // Entry, then trailer, then *junk*. The reader should return the
    // entry, then `None`, even if there are bytes after the trailer.
    let mut archive = build_test_cpio(&[(b"hello", 0o100644, b"hi")]);
    archive.extend_from_slice(b"GARBAGE-AFTER-TRAILER");

    let mut reader = CpioReader::new(&archive);
    let entry = reader.next().expect("entry").expect("ok");
    assert_eq!(entry.name, b"hello");
    assert!(reader.next().is_none(), "trailer terminates iteration");
}

#[test]
fn cpio_reader_returns_bad_magic() {
    // 110 zero bytes followed by anything: parser sees 110 bytes of
    // header, none of which match `070701`/`070702`.
    let archive = vec![0u8; NEWC_HEADER_LEN];
    let mut reader = CpioReader::new(&archive);
    let first = reader.next().expect("first");
    assert!(matches!(first, Err(CpioError::BadMagic)));
}

#[test]
fn cpio_reader_returns_truncated_when_header_short() {
    let archive = vec![b'0'; NEWC_HEADER_LEN - 5];
    let mut reader = CpioReader::new(&archive);
    let first = reader.next().expect("first");
    assert!(matches!(first, Err(CpioError::Truncated)));
}

#[test]
fn cpio_reader_handles_large_namesize_data_alignment() {
    // Build an entry whose name length forces 4-byte padding to
    // engage; verify the parser still finds the right data offset.
    // "abcdefgh" (8 bytes) + trailing NUL = 9 bytes; 110 + 9 = 119,
    // padded to 120 → 1 byte of padding.
    let archive = build_test_cpio(&[(b"abcdefgh", 0o100644, b"DATA")]);
    let mut reader = CpioReader::new(&archive);
    let entry = reader.next().expect("entry").expect("ok");
    assert_eq!(entry.name, b"abcdefgh");
    assert_eq!(entry.data, b"DATA");
}
