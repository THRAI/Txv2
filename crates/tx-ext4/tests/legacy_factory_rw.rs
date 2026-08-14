//! Host oracle for the exact legacy, factory-like ext4 production path.
//!
//! Reproduce with:
//! `cargo test -p tx-ext4 --test legacy_factory_rw factory_like_legacy_image_replays_publicly_encoded_committed_journal -- --exact --nocapture`
//!
//! The test never opens a host block device. It creates one sparse image under
//! the process temporary directory, and every e2fsprogs command names only that
//! image. `mke2fs` cannot create a genuinely active recovery transaction: its
//! new journal has `s_start == 0` and the ext4 `needs_recovery` bit is clear.
//! It also creates an empty legacy journal with no incompat features, so the
//! fixture upgrades only that empty journal superblock to the factory-observed
//! `REVOKE | 64BIT` layout before mount preflight or `e2fsck -fn`.
//!
//! The default Tier 1 entry point and the plain explicit-profile entry point
//! must both reject this legacy image before writing. The production
//! discovered-journal entry point then performs one public namespace mutation,
//! synchronously commits and checkpoints it, and is dropped. Drop is not yet a
//! clean detach: the ext4 `needs_recovery` bit deliberately remains set. A
//! second object is then planned with `Ext4Pager` and encoded solely through
//! the public JBD2 transaction/superblock codecs, leaving a committed journal
//! with non-zero `s_start` and its home blocks untouched. The second discovered
//! mount must replay that object and publish `s_start == 0`. Incomplete-tail,
//! cross-transaction revoke, and multi-descriptor coverage remain in
//! `tx-ext4-format/tests/jbd2_recovery.rs`. This test never clears RECOVER by
//! patching the raw superblock.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tx_ext4::adapter::step_engine::{self as epoch, page_allocator, StepOutcome};
use tx_ext4::journal::JournalPagePool;
use tx_ext4::mount::{
    mount_ext4_read_write, mount_ext4_read_write_with_discovered_journal_profile,
    mount_ext4_read_write_with_profile,
};
use tx_ext4::planner::Ext4BlockGeometry;
use tx_ext4_format::capability::{RwProfile, Tier1MountFacts};
use tx_ext4_format::journal::{Jbd2Features, Jbd2MetadataUpdate, Jbd2TransactionImage};
use tx_ext4_format::mutation::FsyncStamp;
use tx_ext4_format::ondisk::{Ext4FormatError, Superblock};
use tx_ext4_format::pager::{BlockImage, Ext4Pager, InodeNo, JournalGeometry, Page4K, BLOCK_SIZE};
use tx_subsystems::execution::Errno;
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::vfs::structure::FsObjectId;
use tx_subsystems::vfs::Credential;

const IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const LEGACY_COMPAT_EXACT: u32 = 0x0000_003c;
const LEGACY_INCOMPAT_CLEAN_EXACT: u32 = 0x0000_02c2;
const LEGACY_RO_COMPAT_EXACT: u32 = 0x0000_006b;
const JBD2_FEATURE_INCOMPAT_OFFSET: u64 = 40;
const ORACLE_DIRECTORY: &[u8] = b"tx-legacy-oracle";
const REPLAYED_FILE: &[u8] = b"tx-active-replay";

const EXPECTED_FILESYSTEM_FEATURES: &[&str] = &[
    "has_journal",
    "ext_attr",
    "resize_inode",
    "dir_index",
    "filetype",
    "extent",
    "64bit",
    "flex_bg",
    "sparse_super",
    "large_file",
    "huge_file",
    "dir_nlink",
    "extra_isize",
];

const MKE2FS_FEATURES: &str = "none,has_journal,ext_attr,resize_inode,dir_index,filetype,extent,64bit,flex_bg,sparse_super,large_file,huge_file,dir_nlink,extra_isize";

#[test]
fn factory_like_legacy_image_replays_publicly_encoded_committed_journal() {
    let missing = missing_tools(&["mke2fs", "dumpe2fs", "e2fsck"]);
    assert!(
        missing.is_empty(),
        "legacy factory ext4 host oracle requires e2fsprogs tools; missing: {missing:?}"
    );

    let fixture = Fixture::new();
    let image = fixture.path("legacy-factory.ext4");
    let file = File::create(&image).expect("create temporary sparse ext4 image");
    file.set_len(IMAGE_BYTES)
        .expect("size temporary sparse ext4 image");

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

    assert_mke2fs_clean_factory_shape(&image);
    upgrade_empty_journal_to_revoke_64bit(&image);
    assert_exact_factory_shape(&image);
    e2fsck_read_only(&image, "before tx-ext4 mount preflight");

    let facts = mount_facts(&image);
    RwProfile::LegacyNoMetadataCsum
        .admit_mount(facts)
        .expect("exact filesystem facts match the legacy preflight profile");

    let (default_image, default_io) = FileImage::open_counting(&image);
    let default_error = match mount_ext4_read_write(default_image) {
        Ok(_) => panic!("default Tier 1 mount unexpectedly admitted the legacy image"),
        Err(error) => error,
    };
    assert_eq!(default_error, Errno::EOPNOTSUPP);
    assert_eq!(
        default_io.writes.load(Ordering::Acquire),
        0,
        "default Tier 1 rejection must precede every block write"
    );
    assert_eq!(
        default_io.barriers.load(Ordering::Acquire),
        0,
        "default Tier 1 rejection must precede every barrier"
    );

    let (plain_legacy_image, plain_legacy_io) = FileImage::open_counting(&image);
    let plain_legacy_error = match mount_ext4_read_write_with_profile(
        plain_legacy_image,
        RwProfile::LegacyNoMetadataCsum,
    ) {
        Ok(_) => panic!("plain mount bypass unexpectedly admitted the legacy image"),
        Err(error) => error,
    };
    assert_eq!(plain_legacy_error, Errno::EOPNOTSUPP);
    assert_eq!(
        plain_legacy_io.writes.load(Ordering::Acquire),
        0,
        "plain legacy rejection must precede replay, recovery, and every block write"
    );
    assert_eq!(
        plain_legacy_io.barriers.load(Ordering::Acquire),
        0,
        "plain legacy rejection must precede every barrier"
    );

    assert_exact_factory_shape(&image);

    init_substrate();
    let device = DeviceKey::new(7);
    let geometry = Ext4BlockGeometry::new(device, 8);
    let (discovered_image, discovered_io) = FileImage::open_counting(&image);
    let mounted = mount_ext4_read_write_with_discovered_journal_profile(
        discovered_image,
        geometry,
        device,
        JournalPagePool::new(64).expect("journal page pool for factory oracle"),
        RwProfile::LegacyNoMetadataCsum,
    )
    .expect("production discovered-journal mount admits exact legacy image");

    let guard = epoch::guard();
    let credential = Credential::root();
    let (created, created_meta) = match mounted.fs_ops().mkdir(
        FsObjectId::new(2),
        ORACLE_DIRECTORY,
        0o755,
        &credential,
        &guard,
    ) {
        StepOutcome::Done(created) => created,
        other => panic!("public mkdir through discovered-journal mount: {other:?}"),
    };
    assert_eq!(created_meta.mode, 0o40755);
    assert_eq!(created_meta.nlinks, 2);
    assert_eq!(
        mounted
            .fs_ops()
            .lookup(FsObjectId::new(2), ORACLE_DIRECTORY, &guard),
        StepOutcome::done(created)
    );
    drop(guard);
    drop(mounted);
    assert!(
        discovered_io.writes.load(Ordering::Acquire) > 1,
        "discovered mount plus mkdir must publish recovery state and durable journal/checkpoint writes"
    );
    assert!(
        discovered_io.barriers.load(Ordering::Acquire) > 1,
        "discovered mount plus mkdir must cross journal commit and checkpoint barriers"
    );

    assert_recovery_state_and_object(&image, created);
    let replayed = stage_committed_create_in_active_journal(&image);
    assert_active_journal_and_uncheckpointed_object(&image);

    let device = DeviceKey::new(7);
    let geometry = Ext4BlockGeometry::new(device, 8);
    let (reopen_image, reopen_io) = FileImage::open_counting(&image);
    let reopened_mount = mount_ext4_read_write_with_discovered_journal_profile(
        reopen_image,
        geometry,
        device,
        JournalPagePool::new(64).expect("journal page pool for active-replay oracle"),
        RwProfile::LegacyNoMetadataCsum,
    )
    .expect("second discovered mount replays committed active legacy journal");
    let guard = epoch::guard();
    assert_eq!(
        reopened_mount
            .fs_ops()
            .lookup(FsObjectId::new(2), ORACLE_DIRECTORY, &guard),
        StepOutcome::done(created)
    );
    assert_eq!(
        reopened_mount
            .fs_ops()
            .lookup(FsObjectId::new(2), REPLAYED_FILE, &guard),
        StepOutcome::done(replayed)
    );
    drop(guard);
    drop(reopened_mount);
    assert!(
        reopen_io.writes.load(Ordering::Acquire) > 0,
        "active replay mount must write home blocks and republish mounted recovery state"
    );
    assert!(
        reopen_io.barriers.load(Ordering::Acquire) > 0,
        "active replay mount must barrier replay cleanup and recovery-state publication"
    );

    assert_recovery_state_and_object(&image, created);
    assert_replayed_file(&image, replayed);
    // `-n` may replay recovery state in memory, so this is only an external
    // consistency oracle, never evidence of clean detach. Re-open once more to
    // prove the read-only checker did not clear RECOVER on the fixture.
    e2fsck_read_only(&image, "after discovered-mount committed replay round trip");
    assert_recovery_state_and_object(&image, created);
    assert_replayed_file(&image, replayed);
}

fn init_substrate() {
    tx_test_support::init_host();
    tx_test_support::drain_to_quiescence();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones for factory oracle");
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for factory oracle: {error:?}"),
    }
}

fn assert_mke2fs_clean_factory_shape(image: &Path) {
    let mut pager = Ext4Pager::open(FileImage::open(image)).expect("open mke2fs image");
    let superblock = pager.superblock();
    assert_exact_ext4_superblock(superblock);
    let journal = pager
        .journal_geometry()
        .expect("parse mke2fs internal journal geometry");
    assert_eq!(journal.features, Jbd2Features::NONE);
    assert_eq!(journal.superblock.start, 0);
}

fn upgrade_empty_journal_to_revoke_64bit(image: &Path) {
    let mut pager = Ext4Pager::open(FileImage::open(image)).expect("open mke2fs image");
    let journal = pager
        .journal_geometry()
        .expect("discover empty internal journal");
    assert_eq!(
        journal.superblock.start, 0,
        "refuse to patch an active journal"
    );
    let journal_superblock = journal.blocks[0];
    match journal.features {
        Jbd2Features::REVOKE_64BIT => return,
        Jbd2Features::NONE => {}
        observed => panic!("unexpected mke2fs JBD2 feature shape: {observed:?}"),
    }
    drop(pager);

    let offset = journal_superblock
        .checked_mul(BLOCK_SIZE as u64)
        .and_then(|offset| offset.checked_add(JBD2_FEATURE_INCOMPAT_OFFSET))
        .expect("journal feature offset fits u64");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open temporary image to upgrade empty journal features");
    file.seek(SeekFrom::Start(offset))
        .expect("seek JBD2 incompat feature word");
    file.write_all(&Jbd2Features::REVOKE_64BIT.feature_incompat().to_be_bytes())
        .expect("write JBD2 REVOKE|64BIT feature word");
    file.sync_all()
        .expect("persist temporary journal feature upgrade");
}

fn assert_exact_factory_shape(image: &Path) {
    let mut pager = Ext4Pager::open(FileImage::open(image)).expect("open factory-like image");
    let superblock = pager.superblock();
    assert_exact_ext4_superblock(superblock);
    let journal = pager
        .journal_geometry()
        .expect("parse factory-like internal journal");
    assert_eq!(journal.features, Jbd2Features::REVOKE_64BIT);
    assert_eq!(journal.superblock.start, 0);
    drop(pager);

    let output = run_checked(
        "dumpe2fs",
        ["-h", image.to_str().expect("temporary image path is UTF-8")],
    );
    let header = String::from_utf8(output.stdout).expect("dumpe2fs header is UTF-8");
    assert_eq!(
        dumpe2fs_value(&header, "Filesystem features")
            .split_whitespace()
            .collect::<Vec<_>>(),
        EXPECTED_FILESYSTEM_FEATURES
    );
    assert_eq!(dumpe2fs_value(&header, "Filesystem state"), "clean");
    assert_eq!(dumpe2fs_value(&header, "Block size"), "4096");
    assert_eq!(dumpe2fs_value(&header, "Inode size"), "256");
    assert_eq!(dumpe2fs_value(&header, "Journal inode"), "8");
    assert_eq!(
        dumpe2fs_value(&header, "Journal features"),
        "journal_incompat_revoke journal_64bit"
    );
    assert_eq!(dumpe2fs_value(&header, "Journal start"), "0");
}

fn assert_recovery_state_and_object(image: &Path, expected: FsObjectId) {
    let mut pager =
        Ext4Pager::open(FileImage::open_read_only(image)).expect("read-only reopen after drop");
    let superblock = pager.superblock();
    assert_exact_ext4_superblock_with_incompat(
        superblock,
        LEGACY_INCOMPAT_CLEAN_EXACT | Superblock::FEATURE_INCOMPAT_RECOVER,
    );
    assert!(superblock.needs_recovery());

    let inode = pager
        .lookup(InodeNo::new(2), ORACLE_DIRECTORY)
        .expect("read-only lookup of journaled directory")
        .expect("journaled directory persists after drop");
    assert_eq!(u64::from(inode.get()), expected.as_u64());
    let meta = pager
        .inode_meta(inode)
        .expect("read-only metadata for journaled directory");
    assert_eq!(meta.mode, 0o40755);
    assert_eq!(meta.nlinks, 2);

    let journal = pager
        .journal_geometry()
        .expect("read-only journal state after checkpoint");
    assert_eq!(journal.features, Jbd2Features::REVOKE_64BIT);
    assert_eq!(
        journal.superblock.start, 0,
        "namespace success must include commit/checkpoint settlement"
    );
}

fn stage_committed_create_in_active_journal(image: &Path) -> FsObjectId {
    let mut pager = Ext4Pager::open(FileImage::open(image)).expect("open checkpointed image");
    assert!(pager.superblock().needs_recovery());
    let geometry = pager
        .journal_geometry()
        .expect("discover clean journal before active transaction");
    assert_eq!(geometry.features, Jbd2Features::REVOKE_64BIT);
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
        .expect("plan uncheckpointed replay file");
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
    .expect("encode committed active transaction through public JBD2 codec");
    let mut block_image = pager.into_inner();
    publish_committed_transaction(&mut block_image, &geometry, transaction);
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
    assert!(
        record_pages < ring_capacity,
        "publicly encoded transaction must not wrap over its active start"
    );

    let start = geometry.superblock.first;
    let mut active_superblock = geometry
        .superblock_page
        .expect("discovered journal retains its superblock page");
    geometry
        .superblock
        .write_state(&mut active_superblock, geometry.superblock.sequence, start)
        .expect("publish active journal state through public codec");
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
    image
        .barrier()
        .expect("barrier descriptor and after-images");
    write_journal_record(image, geometry, &mut logical, &transaction.commit);
    image
        .barrier()
        .expect("barrier committed journal transaction");
}

fn write_journal_record(
    image: &mut FileImage,
    geometry: &JournalGeometry,
    logical: &mut u32,
    page: &Page4K,
) {
    assert!(*logical >= geometry.superblock.first && *logical < geometry.superblock.max_len);
    let physical = geometry.blocks[*logical as usize];
    image
        .write_block(physical, page)
        .expect("write publicly encoded journal record");
    *logical += 1;
    if *logical == geometry.superblock.max_len {
        *logical = geometry.superblock.first;
    }
}

fn assert_active_journal_and_uncheckpointed_object(image: &Path) {
    let mut pager = Ext4Pager::open(FileImage::open_read_only(image))
        .expect("read-only reopen of active committed journal");
    assert!(pager.superblock().needs_recovery());
    assert_eq!(
        pager
            .lookup(InodeNo::new(2), REPLAYED_FILE)
            .expect("home directory remains readable before replay"),
        None,
        "planned object must exist only in the committed journal before replay"
    );
    let geometry = pager
        .journal_geometry()
        .expect("parse active committed journal");
    assert_eq!(geometry.features, Jbd2Features::REVOKE_64BIT);
    assert_ne!(geometry.superblock.start, 0);
}

fn assert_replayed_file(image: &Path, expected: FsObjectId) {
    let mut pager =
        Ext4Pager::open(FileImage::open_read_only(image)).expect("read-only replay file reopen");
    let inode = pager
        .lookup(InodeNo::new(2), REPLAYED_FILE)
        .expect("lookup replayed file")
        .expect("committed journal replay publishes file");
    assert_eq!(u64::from(inode.get()), expected.as_u64());
    let meta = pager.inode_meta(inode).expect("metadata for replayed file");
    assert_eq!(meta.mode, 0o100644);
    assert_eq!(meta.nlinks, 1);
}

fn assert_exact_ext4_superblock(superblock: Superblock) {
    assert_exact_ext4_superblock_with_incompat(superblock, LEGACY_INCOMPAT_CLEAN_EXACT);
}

fn assert_exact_ext4_superblock_with_incompat(superblock: Superblock, expected_incompat: u32) {
    assert_eq!(superblock.block_size(), BLOCK_SIZE as u32);
    assert_eq!(superblock.inode_size, 256);
    assert_eq!(superblock.feature_compat, LEGACY_COMPAT_EXACT);
    assert_eq!(
        superblock.feature_incompat, expected_incompat,
        "factory oracle requires the exact stable incompat mask plus only the expected recovery state"
    );
    assert_eq!(superblock.feature_ro_compat, LEGACY_RO_COMPAT_EXACT);
    assert_eq!(superblock.journal_inode, 8);
    assert_eq!(superblock.last_orphan, 0);
    assert!(!superblock.has_metadata_csum());
}

fn mount_facts(image: &Path) -> Tier1MountFacts {
    let pager = Ext4Pager::open(FileImage::open(image)).expect("open image for mount facts");
    Tier1MountFacts::from_superblock(&pager.superblock())
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
    io: Arc<IoCounters>,
}

#[derive(Default)]
struct IoCounters {
    writes: AtomicU64,
    barriers: AtomicU64,
}

impl FileImage {
    fn open(path: &Path) -> Self {
        Self::open_with_counters(path, Arc::new(IoCounters::default()))
    }

    fn open_counting(path: &Path) -> (Self, Arc<IoCounters>) {
        let io = Arc::new(IoCounters::default());
        (Self::open_with_counters(path, Arc::clone(&io)), io)
    }

    fn open_read_only(path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .expect("open temporary ext4 image read-only");
        let len = file.metadata().expect("stat temporary ext4 image").len();
        assert_eq!(len % BLOCK_SIZE as u64, 0);
        Self {
            file: RefCell::new(file),
            blocks: len / BLOCK_SIZE as u64,
            io: Arc::new(IoCounters::default()),
        }
    }

    fn open_with_counters(path: &Path, io: Arc<IoCounters>) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open temporary ext4 image");
        let len = file.metadata().expect("stat temporary ext4 image").len();
        assert_eq!(len % BLOCK_SIZE as u64, 0);
        Self {
            file: RefCell::new(file),
            blocks: len / BLOCK_SIZE as u64,
            io,
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
        self.io.writes.fetch_add(1, Ordering::AcqRel);
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(data))
            .map_err(|_| Ext4FormatError::Corrupt)
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.io.barriers.fetch_add(1, Ordering::AcqRel);
        self.file
            .borrow_mut()
            .sync_data()
            .map_err(|_| Ext4FormatError::Corrupt)
    }
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

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let serial = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tx-ext4-legacy-factory-{}-{nonce}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create temporary legacy ext4 fixture directory");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove temporary legacy ext4 fixture directory");
    }
}
