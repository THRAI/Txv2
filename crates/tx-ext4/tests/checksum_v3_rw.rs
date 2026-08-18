//! Linux/e2fsprogs interoperability oracle for the production checksum-v3 RW path.
//!
//! The fixture is built by `mke2fs`; `debugfs`'s public journal commands enable
//! checksum-v3 and create/replay a revoke-only transaction so the resulting
//! clean journal genuinely carries REVOKE | 64BIT | CSUM_V3. No journal field
//! or checksum is patched by byte offset.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use tx_ext4::adapter::step_engine::{self as epoch, page_allocator, StepOutcome};
use tx_ext4::journal::JournalPagePool;
use tx_ext4::mount::mount_ext4_read_write_with_discovered_journal_profile;
use tx_ext4::planner::Ext4BlockGeometry;
use tx_ext4_format::capability::RwProfile;
use tx_ext4_format::journal::{Jbd2MetadataUpdate, Jbd2TransactionImage};
use tx_ext4_format::mutation::FsyncStamp;
use tx_ext4_format::ondisk::{Ext4FormatError, Superblock};
use tx_ext4_format::pager::{BlockImage, Ext4Pager, InodeNo, JournalGeometry, Page4K, BLOCK_SIZE};
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::vfs::structure::FsObjectId;
use tx_subsystems::vfs::Credential;

const IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const JBD2_REVOKE_64BIT_CSUM_V3: u32 = 0x0000_0013;
const MKE2FS_FEATURES: &str = "none,has_journal,ext_attr,resize_inode,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,extra_isize,metadata_csum";
const ORACLE_DIRECTORY: &[u8] = b"tx-csum-v3-oracle";
const REPLAYED_FILE: &[u8] = b"tx-csum-v3-replay";

#[test]
fn linux_checksum_v3_image_round_trips_production_rw_and_active_replay() {
    let missing = missing_tools(&["mke2fs", "debugfs", "dumpe2fs", "e2fsck"]);
    assert!(
        missing.is_empty(),
        "checksum-v3 ext4 host oracle requires e2fsprogs tools; missing: {missing:?}"
    );

    let fixture = Fixture::new();
    let image = fixture.path("checksum-v3.ext4");
    create_linux_checksum_v3_image(&image);
    assert_linux_checksum_v3_shape(&image);
    e2fsck_read_only(&image, "before tx-ext4 mount");

    init_substrate();
    let device = DeviceKey::new(17);
    let block_geometry = Ext4BlockGeometry::new(device, 8);
    let block_image = FileImage::open(&image);
    let mounted = mount_ext4_read_write_with_discovered_journal_profile(
        block_image,
        block_geometry,
        device,
        JournalPagePool::new(64).expect("journal page pool for checksum-v3 oracle"),
        RwProfile::Tier1,
    )
    .expect("production Tier1 mount admits Linux checksum-v3 journal");

    let guard = epoch::guard();
    let credential = Credential::root();
    let (created, metadata) = match mounted.fs_ops().mkdir(
        FsObjectId::new(2),
        ORACLE_DIRECTORY,
        0o755,
        &credential,
        &guard,
    ) {
        StepOutcome::Done(created) => created,
        other => panic!("public mkdir through checksum-v3 mount: {other:?}"),
    };
    assert_eq!(metadata.mode, 0o40755);
    assert_eq!(metadata.nlinks, 2);
    assert_eq!(
        mounted
            .fs_ops()
            .lookup(FsObjectId::new(2), ORACLE_DIRECTORY, &guard),
        StepOutcome::done(created)
    );
    drop(guard);
    drop(mounted);

    assert_read_only_object_and_clean_journal(&image, ORACLE_DIRECTORY, created, 0o40755);
    let replayed = stage_committed_create_in_active_journal(&image);
    assert_active_journal_and_uncheckpointed_object(&image);

    let device = DeviceKey::new(17);
    let block_geometry = Ext4BlockGeometry::new(device, 8);
    let block_image = FileImage::open(&image);
    let remounted = mount_ext4_read_write_with_discovered_journal_profile(
        block_image,
        block_geometry,
        device,
        JournalPagePool::new(64).expect("journal page pool for checksum-v3 replay"),
        RwProfile::Tier1,
    )
    .expect("production Tier1 mount replays active checksum-v3 transaction");
    let guard = epoch::guard();
    assert_eq!(
        remounted
            .fs_ops()
            .lookup(FsObjectId::new(2), ORACLE_DIRECTORY, &guard),
        StepOutcome::done(created)
    );
    assert_eq!(
        remounted
            .fs_ops()
            .lookup(FsObjectId::new(2), REPLAYED_FILE, &guard),
        StepOutcome::done(replayed)
    );
    drop(guard);
    drop(remounted);

    assert_read_only_object_and_clean_journal(&image, REPLAYED_FILE, replayed, 0o100644);
    assert_linux_checksum_v3_shape(&image);
    // Mount drop is not a clean detach: tx-ext4 intentionally retains the
    // ext4 RECOVER bit. `e2fsck -fn` therefore remains a read-only consistency
    // oracle, not evidence that the filesystem was cleanly unmounted.
    e2fsck_read_only(&image, "after checksum-v3 mutation and committed replay");
    assert_read_only_object_and_clean_journal(&image, REPLAYED_FILE, replayed, 0o100644);
}

fn create_linux_checksum_v3_image(image: &Path) {
    let file = File::create(image).expect("create sparse checksum-v3 ext4 image");
    file.set_len(IMAGE_BYTES)
        .expect("size sparse checksum-v3 ext4 image");
    run_checked(
        "mke2fs",
        [
            "-q",
            "-t",
            "ext4",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            MKE2FS_FEATURES,
            "-E",
            "lazy_itable_init=0,lazy_journal_init=0",
            image.to_str().expect("temporary image path is UTF-8"),
        ],
    );

    run_debugfs(
        image,
        "journal_open -c -v 3\n\
         journal_write -r 100 /dev/null\n\
         journal_close\n\
         journal_run\n",
    );
}

fn assert_linux_checksum_v3_shape(image: &Path) {
    assert_eq!(fs::metadata(image).unwrap().len(), IMAGE_BYTES);
    let output = run_checked(
        "dumpe2fs",
        ["-h", image.to_str().expect("temporary image path is UTF-8")],
    );
    let header = String::from_utf8(output.stdout).expect("dumpe2fs header is UTF-8");
    let filesystem_features = dumpe2fs_value(&header, "Filesystem features")
        .split_whitespace()
        .collect::<Vec<_>>();
    assert!(filesystem_features.contains(&"64bit"));
    assert!(filesystem_features.contains(&"metadata_csum"));
    assert_eq!(dumpe2fs_value(&header, "Block size"), "4096");
    assert_eq!(dumpe2fs_value(&header, "Inode size"), "256");
    assert_eq!(dumpe2fs_value(&header, "Journal inode"), "8");
    assert_eq!(
        dumpe2fs_value(&header, "Journal features")
            .split_whitespace()
            .collect::<Vec<_>>(),
        [
            "journal_incompat_revoke",
            "journal_64bit",
            "journal_checksum_v3",
        ]
    );
    assert_eq!(dumpe2fs_value(&header, "Journal checksum type"), "crc32c");

    let pager = Ext4Pager::open(FileImage::open_read_only(image))
        .expect("open Linux checksum-v3 image read-only");
    let superblock = pager.superblock();
    assert_eq!(superblock.block_size(), BLOCK_SIZE as u32);
    assert_eq!(superblock.inode_size, 256);
    assert!(superblock.has_metadata_csum());
    assert_ne!(
        superblock.feature_incompat & Superblock::FEATURE_INCOMPAT_64BIT,
        0
    );
}

fn stage_committed_create_in_active_journal(image: &Path) -> FsObjectId {
    let mut pager = Ext4Pager::open(FileImage::open(image)).expect("open checkpointed image");
    assert!(pager.superblock().needs_recovery());
    let geometry = pager
        .journal_geometry()
        .expect("discover clean checksum-v3 journal");
    assert_eq!(
        geometry.features.feature_incompat(),
        JBD2_REVOKE_64BIT_CSUM_V3
    );
    assert_eq!(geometry.superblock.start, 0);

    let (inode, mutation) = pager
        .plan_create_regular_file(
            InodeNo::new(2),
            REPLAYED_FILE,
            0o644,
            0,
            0,
            FsyncStamp::new(1),
        )
        .expect("plan uncheckpointed checksum-v3 replay file");
    assert!(mutation.data.is_empty());
    let updates = mutation
        .metadata
        .iter()
        .map(|metadata| Jbd2MetadataUpdate::new64(metadata.home, metadata.after))
        .collect();
    let revokes = mutation
        .revokes
        .iter()
        .map(|revoke| revoke.physical_block)
        .collect();
    let transaction = Jbd2TransactionImage::encode_with_features_and_revokes(
        geometry.superblock.sequence,
        geometry.superblock.uuid,
        updates,
        revokes,
        geometry.features,
    )
    .expect("encode active transaction through public checksum-v3 codec");
    let mut image = pager.into_inner();
    publish_committed_transaction(&mut image, &geometry, transaction);
    FsObjectId::new(u64::from(inode.get()))
}

fn publish_committed_transaction(
    image: &mut FileImage,
    geometry: &JournalGeometry,
    transaction: Jbd2TransactionImage,
) {
    let record_pages = 2 + transaction.metadata_blocks.len() + transaction.revokes.len();
    let ring_capacity = usize::try_from(geometry.superblock.max_len - geometry.superblock.first)
        .expect("journal ring capacity fits usize");
    assert!(record_pages < ring_capacity);

    let start = geometry.superblock.first;
    let mut active_superblock = geometry
        .superblock_page
        .expect("discovered journal retains its superblock page");
    geometry
        .superblock
        .write_state(&mut active_superblock, geometry.superblock.sequence, start)
        .expect("publish active checksum-v3 journal state");
    image
        .write_block(geometry.blocks[0], &active_superblock)
        .expect("write active journal superblock");
    image.barrier().expect("barrier active journal publication");

    let mut logical = start;
    write_journal_record(image, geometry, &mut logical, &transaction.descriptor);
    for page in &transaction.metadata_blocks {
        write_journal_record(image, geometry, &mut logical, page);
    }
    for page in &transaction.revokes {
        write_journal_record(image, geometry, &mut logical, page);
    }
    image.barrier().expect("barrier journal after-images");
    write_journal_record(image, geometry, &mut logical, &transaction.commit);
    image.barrier().expect("barrier committed transaction");
}

fn write_journal_record(
    image: &mut FileImage,
    geometry: &JournalGeometry,
    logical: &mut u32,
    page: &Page4K,
) {
    let physical = geometry.blocks[*logical as usize];
    image
        .write_block(physical, page)
        .expect("write publicly encoded checksum-v3 journal record");
    *logical += 1;
    if *logical == geometry.superblock.max_len {
        *logical = geometry.superblock.first;
    }
}

fn assert_active_journal_and_uncheckpointed_object(image: &Path) {
    let mut pager = Ext4Pager::open(FileImage::open_read_only(image))
        .expect("read-only reopen of active checksum-v3 journal");
    assert!(pager.superblock().needs_recovery());
    assert_eq!(
        pager
            .lookup(InodeNo::new(2), REPLAYED_FILE)
            .expect("read home directory before replay"),
        None
    );
    let journal = pager.journal_geometry().expect("parse active journal");
    assert_eq!(
        journal.features.feature_incompat(),
        JBD2_REVOKE_64BIT_CSUM_V3
    );
    assert_ne!(journal.superblock.start, 0);
}

fn assert_read_only_object_and_clean_journal(
    image: &Path,
    name: &[u8],
    expected: FsObjectId,
    expected_mode: u16,
) {
    let mut pager = Ext4Pager::open(FileImage::open_read_only(image))
        .expect("read-only reopen after checksum-v3 mount drop");
    assert!(
        pager.superblock().needs_recovery(),
        "mount drop is not a clean detach and must retain RECOVER"
    );
    let inode = pager
        .lookup(InodeNo::new(2), name)
        .expect("read-only object lookup")
        .expect("journaled object persists after drop");
    assert_eq!(u64::from(inode.get()), expected.as_u64());
    assert_eq!(pager.inode_meta(inode).unwrap().mode, expected_mode);
    let journal = pager.journal_geometry().expect("read-only journal state");
    assert_eq!(
        journal.features.feature_incompat(),
        JBD2_REVOKE_64BIT_CSUM_V3
    );
    assert_eq!(journal.superblock.start, 0);
}

fn init_substrate() {
    tx_test_support::init_host();
    tx_test_support::drain_to_quiescence();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones for checksum-v3 oracle");
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for checksum-v3 oracle: {error:?}"),
    }
}

fn e2fsck_read_only(image: &Path, phase: &str) {
    let output = run_checked(
        "e2fsck",
        [
            "-fn",
            image.to_str().expect("temporary image path is UTF-8"),
        ],
    );
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !transcript.contains("UNEXPECTED INCONSISTENCY")
            && !transcript.contains("Filesystem still has errors"),
        "e2fsck -fn reported corruption {phase}:\n{transcript}"
    );
}

fn dumpe2fs_value<'a>(header: &'a str, key: &str) -> &'a str {
    header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(candidate, value)| (candidate.trim() == key).then_some(value.trim()))
        .unwrap_or_else(|| panic!("dumpe2fs field {key:?} missing from:\n{header}"))
}

struct FileImage {
    file: RefCell<File>,
    blocks: u64,
}

impl FileImage {
    fn open(path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Self::from_file(file)
    }

    fn open_read_only(path: &Path) -> Self {
        let file = OpenOptions::new().read(true).open(path).unwrap();
        Self::from_file(file)
    }

    fn from_file(file: File) -> Self {
        let len = file.metadata().unwrap().len();
        assert_eq!(len % BLOCK_SIZE as u64, 0);
        Self {
            file: RefCell::new(file),
            blocks: len / BLOCK_SIZE as u64,
        }
    }

    fn offset(&self, block: u64) -> tx_ext4_format::Result<u64> {
        if block >= self.blocks {
            return Err(Ext4FormatError::OutOfBounds);
        }
        block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Ext4FormatError::OutOfBounds)
    }
}

impl BlockImage for FileImage {
    fn total_blocks(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        let offset = self.offset(block)?;
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(out))
            .map_err(|_| Ext4FormatError::Corrupt)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let offset = self.offset(block)?;
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(data))
            .map_err(|_| Ext4FormatError::Corrupt)
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.file
            .borrow_mut()
            .sync_data()
            .map_err(|_| Ext4FormatError::Corrupt)
    }
}

fn run_debugfs(image: &Path, script: &str) {
    let mut child = Command::new("debugfs")
        .arg("-w")
        .arg(image)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run debugfs");
    child
        .stdin
        .take()
        .expect("debugfs stdin")
        .write_all(script.as_bytes())
        .expect("send journal commands to debugfs");
    let output = child.wait_with_output().expect("wait for debugfs");
    assert!(
        output.status.success(),
        "debugfs journal setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

fn run_checked<I, S>(program: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tx-ext4-checksum-v3-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create checksum-v3 fixture directory");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove checksum-v3 fixture directory");
    }
}
