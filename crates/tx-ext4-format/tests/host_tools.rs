use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use tx_ext4_format::capability::{Tier1Capabilities, Tier1Reject, Tier1Request, sha256};
use tx_ext4_format::ondisk::Ext4FormatError;
use tx_ext4_format::pager::{BLOCK_SIZE, BlockImage, DirEntryLite, Ext4Pager, InodeNo};

#[test]
fn tier1_rejects_unsupported_shape_before_mutation() {
    let profile = Tier1Capabilities::generated();
    for request in [
        Tier1Request::ExtentDepthGrowth,
        Tier1Request::HtreeSplit,
        Tier1Request::OrphanFile,
        Tier1Request::DirectIo,
    ] {
        assert_eq!(profile.admit(request), Err(Tier1Reject::Unsupported));
    }
    for request in [
        Tier1Request::DepthOneExtent,
        Tier1Request::LinearDirectory,
        Tier1Request::NonSplittingHtree,
        Tier1Request::ClassicOrphan,
    ] {
        assert_eq!(profile.admit(request), Ok(()));
    }
}

#[test]
fn tier1_generated_capability_profile_matches_authority_ledger() {
    let ledger = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/ext4/tier1/capability-ledger.json"
    ));
    assert_eq!(
        Tier1Capabilities::generated().profile_hash(),
        sha256(ledger)
    );
}

struct VecImage {
    bytes: Vec<u8>,
}

impl VecImage {
    fn open(path: &Path) -> Self {
        Self {
            bytes: fs::read(path).expect("read generated ext4 image"),
        }
    }
}

impl BlockImage for VecImage {
    fn total_blocks(&self) -> u64 {
        (self.bytes.len() / BLOCK_SIZE) as u64
    }

    fn read_block(&self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let start = block as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        out.copy_from_slice(bytes);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let start = block as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        let bytes = self
            .bytes
            .get_mut(start..end)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        bytes.copy_from_slice(data);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        Ok(())
    }
}

#[test]
fn generated_ext4_image_matches_host_tool_directory_and_file_observations() {
    let missing = missing_tools(&["mkfs.ext4", "debugfs", "dumpe2fs", "e2fsck"]);
    if !missing.is_empty() {
        eprintln!("skipping host-tool ext4 verification; missing: {missing:?}");
        return;
    }

    let fixture = Fixture::new();
    let image = fixture.path("image.ext4");
    let payload = fixture.path("hello.payload");
    let payload_bytes = b"hello from debugfs-created ext4 image\n";
    fs::write(&payload, payload_bytes).expect("write payload fixture");

    let file = File::create(&image).expect("create ext4 image");
    file.set_len(32 * 1024 * 1024).expect("size ext4 image");

    run(
        "mkfs.ext4",
        [
            "-q",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "extent,^64bit,^metadata_csum",
            "-E",
            "lazy_itable_init=0,lazy_journal_init=0",
            image.to_str().unwrap(),
        ],
    );
    run(
        "debugfs",
        ["-w", "-R", "mkdir /folder", image.to_str().unwrap()],
    );
    let write_cmd = format!("write {} /folder/hello.txt", payload.display());
    run(
        "debugfs",
        ["-w", "-R", write_cmd.as_str(), image.to_str().unwrap()],
    );
    run("e2fsck", ["-fn", image.to_str().unwrap()]);

    let dumpe2fs = run("dumpe2fs", ["-h", image.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&dumpe2fs.stdout).contains("Block size:               4096"));

    let folder_stat = debugfs_stat(&image, "/folder");
    let file_stat = debugfs_stat(&image, "/folder/hello.txt");
    let folder_ino = InodeNo::new(parse_debugfs_u32(&folder_stat, "Inode:"));
    let file_ino = InodeNo::new(parse_debugfs_u32(&file_stat, "Inode:"));
    let debugfs_size = parse_debugfs_u64(&file_stat, "Size:");

    let mut pager = Ext4Pager::open(VecImage::open(&image)).expect("open generated ext4 image");
    assert_eq!(
        pager.lookup(InodeNo::new(2), b"folder").unwrap(),
        Some(folder_ino)
    );
    assert_eq!(
        pager.lookup(folder_ino, b"hello.txt").unwrap(),
        Some(file_ino)
    );

    let file_meta = pager.inode_meta(file_ino).unwrap();
    assert_eq!(file_meta.size, debugfs_size);
    assert_eq!(file_meta.size, payload_bytes.len() as u64);

    let mut entries = [DirEntryLite::empty(); 8];
    let count = pager.read_dir_entries(folder_ino, &mut entries).unwrap();
    assert!(
        entries[..count]
            .iter()
            .any(|entry| entry.name() == b"hello.txt")
    );

    let mut page = [0u8; BLOCK_SIZE];
    pager.read_page(file_ino, 0, &mut page).unwrap();
    assert_eq!(&page[..payload_bytes.len()], payload_bytes);
    assert!(page[payload_bytes.len()..].iter().all(|byte| *byte == 0));
}

fn debugfs_stat(image: &Path, path: &str) -> String {
    let command = format!("stat {path}");
    let output = run("debugfs", ["-R", command.as_str(), image.to_str().unwrap()]);
    String::from_utf8(output.stdout).expect("debugfs stat utf8")
}

fn parse_debugfs_u32(output: &str, key: &str) -> u32 {
    parse_debugfs_u64(output, key)
        .try_into()
        .expect("debugfs value fits u32")
}

fn parse_debugfs_u64(output: &str, key: &str) -> u64 {
    let mut words = output.split_whitespace();
    while let Some(word) = words.next() {
        if word == key {
            return words
                .next()
                .expect("debugfs value after key")
                .parse()
                .expect("debugfs numeric value");
        }
    }
    panic!("debugfs key {key:?} not found in:\n{output}");
}

fn missing_tools(tools: &[&str]) -> Vec<String> {
    tools
        .iter()
        .copied()
        .filter(|tool| !command_exists(tool))
        .map(str::to_owned)
        .collect()
}

fn command_exists(tool: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|path| path.join(tool).is_file())
}

fn run<I, S>(program: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run {program}: {err}"));
    if !output.status.success() {
        panic!(
            "{program} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tx-ext4-format-{}-{unique}", std::process::id()));
        fs::create_dir(&root).expect("create fixture temp dir");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
