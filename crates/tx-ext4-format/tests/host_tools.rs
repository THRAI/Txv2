use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use tx_ext4_format::capability::{
    sha256, RwProfile, Tier1Capabilities, Tier1MountFacts, Tier1Reject, Tier1Request,
    LEGACY_NO_METADATA_CSUM_PROFILE_DESCRIPTOR,
};
use tx_ext4_format::mutation::{FsyncStamp, MutationOrigin};
use tx_ext4_format::ondisk::{Ext4FormatError, ExtentHeader, ExtentNode, Superblock};
use tx_ext4_format::pager::{BlockImage, DirEntryLite, Ext4Pager, InodeNo, BLOCK_SIZE};

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

#[test]
fn legacy_no_metadata_csum_profile_admits_only_the_exact_factory_shape() {
    let profile = RwProfile::LegacyNoMetadataCsum;
    let facts = legacy_no_metadata_csum_facts();

    assert_eq!(
        profile.capabilities().profile_hash(),
        sha256(LEGACY_NO_METADATA_CSUM_PROFILE_DESCRIPTOR)
    );
    assert_eq!(
        profile.admit_mount(facts),
        Ok(tx_ext4_format::capability::CapabilityProfileHash(
            profile.capabilities().profile_hash()
        ))
    );

    let mut clean = facts;
    clean.feature_bits.incompat &= !Superblock::FEATURE_INCOMPAT_RECOVER;
    assert!(profile.admit_mount(clean).is_ok());

    let mut missing_stable_feature = facts;
    missing_stable_feature.feature_bits.compat &= !0x0000_0008;
    assert_eq!(
        profile.admit_mount(missing_stable_feature),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut metadata_csum = facts;
    metadata_csum.feature_bits.ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    metadata_csum.feature_bits.metadata_csum = true;
    assert_eq!(
        profile.admit_mount(metadata_csum),
        Err(Tier1Reject::ProfileMismatch)
    );
}

#[test]
fn legacy_no_metadata_csum_profile_rejects_unknown_bits_and_non_exact_geometry() {
    let profile = RwProfile::LegacyNoMetadataCsum;
    let facts = legacy_no_metadata_csum_facts();

    let mut unknown_compat = facts;
    unknown_compat.feature_bits.compat |= 0x8000_0000;
    assert_eq!(
        profile.admit_mount(unknown_compat),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut unknown_incompat = facts;
    unknown_incompat.feature_bits.incompat |= 0x8000_0000;
    assert_eq!(
        profile.admit_mount(unknown_incompat),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut unknown_ro_compat = facts;
    unknown_ro_compat.feature_bits.ro_compat |= 0x8000_0000;
    assert_eq!(
        profile.admit_mount(unknown_ro_compat),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut inode_128 = facts;
    inode_128.geometry.inode_size = 128;
    assert_eq!(
        profile.admit_mount(inode_128),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut block_1k = facts;
    block_1k.geometry.block_size = 1024;
    assert_eq!(
        profile.admit_mount(block_1k),
        Err(Tier1Reject::ProfileMismatch)
    );

    let mut external_journal = facts;
    external_journal.feature_bits.ordered_jbd2 = false;
    assert_eq!(
        profile.admit_mount(external_journal),
        Err(Tier1Reject::ProfileMismatch)
    );
}

#[test]
fn legacy_no_metadata_csum_profile_requires_an_internal_journal() {
    let profile = RwProfile::LegacyNoMetadataCsum;
    let mut superblock = legacy_no_metadata_csum_superblock();
    assert!(profile
        .admit_mount(Tier1MountFacts::from_superblock(&superblock))
        .is_ok());

    superblock.journal_inode = 0;
    assert_eq!(
        profile.admit_mount(Tier1MountFacts::from_superblock(&superblock)),
        Err(Tier1Reject::ProfileMismatch)
    );

    superblock = legacy_no_metadata_csum_superblock();
    // journal_dev is an external-journal filesystem shape and is not in the
    // exact legacy filesystem incompat mask.
    superblock.feature_incompat |= 0x0000_0008;
    assert_eq!(
        profile.admit_mount(Tier1MountFacts::from_superblock(&superblock)),
        Err(Tier1Reject::ProfileMismatch)
    );
}

#[test]
fn rw_profile_default_remains_tier1_without_legacy_fallback() {
    let facts = legacy_no_metadata_csum_facts();

    assert_eq!(RwProfile::default(), RwProfile::Tier1);
    assert_eq!(
        Tier1Capabilities::generated().admit_mount(facts),
        Err(Tier1Reject::ProfileMismatch)
    );
    assert_ne!(
        RwProfile::Tier1.capabilities().profile_hash(),
        RwProfile::LegacyNoMetadataCsum
            .capabilities()
            .profile_hash()
    );
}

fn legacy_no_metadata_csum_facts() -> Tier1MountFacts {
    Tier1MountFacts::from_superblock(&legacy_no_metadata_csum_superblock())
}

fn legacy_no_metadata_csum_superblock() -> Superblock {
    Superblock {
        log_block_size: 2,
        inode_size: 256,
        feature_compat:
            // has_journal | ext_attr | resize_inode | dir_index
            0x0000_003c,
        // filetype | recover | extents | 64bit | flex_bg
        feature_incompat: 0x0000_02c6,
        // sparse_super | large_file | huge_file | dir_nlink | extra_isize
        feature_ro_compat: 0x0000_006b,
        journal_inode: 8,
        ..Superblock::default()
    }
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

struct FileImage {
    file: RefCell<File>,
    total_blocks: u64,
}

impl FileImage {
    fn open(path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open generated ext4 image");
        let total_blocks =
            file.metadata().expect("stat generated ext4 image").len() / BLOCK_SIZE as u64;
        Self {
            file: RefCell::new(file),
            total_blocks,
        }
    }
}

impl BlockImage for FileImage {
    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    fn read_block(&self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let offset = block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(out))
            .map_err(|_| Ext4FormatError::Corrupt)
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let offset = block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Ext4FormatError::OutOfBounds)?;
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
    assert!(entries[..count]
        .iter()
        .any(|entry| entry.name() == b"hello.txt"));

    let mut page = [0u8; BLOCK_SIZE];
    pager.read_page(file_ino, 0, &mut page).unwrap();
    assert_eq!(&page[..payload_bytes.len()], payload_bytes);
    assert!(page[payload_bytes.len()..].iter().all(|byte| *byte == 0));
}

#[test]
fn tier1_busybox_image_can_plan_mkdir_under_musl() {
    let missing = missing_tools(&["mkfs.ext4", "e2fsck"]);
    if !missing.is_empty() {
        eprintln!("skipping tier1 image mkdir planner verification; missing: {missing:?}");
        return;
    }

    let fixture = Fixture::new();
    let image = fixture.path("busybox.ext4");
    let layout = fixture.path("layout");
    fs::create_dir_all(layout.join("musl")).expect("create tier1 root layout");
    fs::write(layout.join("musl").join("busybox"), b"busybox fixture\n")
        .expect("write busybox fixture");

    let file = File::create(&image).expect("create ext4 image");
    file.set_len(64 * 1024 * 1024).expect("size ext4 image");

    run(
        &command_or_candidates("mkfs.ext4").expect("mkfs.ext4 tool"),
        [
            "-q",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "^orphan_file,^metadata_csum_seed",
            "-E",
            "lazy_itable_init=0,lazy_journal_init=0",
            "-L",
            "TXROOT",
            "-d",
            layout.to_str().unwrap(),
            image.to_str().unwrap(),
        ],
    );
    run(
        &command_or_candidates("e2fsck").expect("e2fsck tool"),
        ["-fn", image.to_str().unwrap()],
    );

    let mut pager = Ext4Pager::open(VecImage::open(&image)).expect("open tier1 image");
    let musl = pager
        .lookup(InodeNo::new(2), b"musl")
        .expect("lookup /musl")
        .expect("/musl exists");
    let (new_ino, data_block, plan) = pager
        .plan_create_directory(musl, b"tier1-data", 0o755, 0, 0, FsyncStamp::new(0))
        .expect("plan mkdir under /musl");

    assert!(new_ino.get() > 2);
    assert!(data_block > 0);
    assert_eq!(plan.origin, MutationOrigin::Create);
    assert!(plan.metadata.len() >= 7);
}

#[test]
fn docker_e2fsck_accepts_inline_root_spill_after_images() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 verification; image unavailable: {image_name}");
        return;
    }

    let fixture = Fixture::docker_mountable();
    let image = fixture.path("inline-root-spill.ext4");
    docker_build_inline_root_fixture(&image_name, &fixture, &image);

    let mut pager = Ext4Pager::open(VecImage::open(&image)).expect("open Docker ext4 image");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Docker fixture")
        .expect("/file exists");
    let page = [0xE9; BLOCK_SIZE];
    let plan = pager
        .plan_write_page(inode, 8, &page, FsyncStamp::new(52))
        .expect("plan inline-root spill");
    assert_eq!(plan.allocations.len(), 2);
    assert_eq!(
        plan.metadata
            .iter()
            .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
            .count(),
        1
    );

    for data in &plan.data {
        pager
            .apply_l6_write_page(data.physical_block * 8, 8, &data.bytes)
            .expect("write planned data page");
    }
    pager
        .apply_l6_barrier()
        .expect("order data before metadata");
    for metadata in &plan.metadata {
        pager
            .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
            .expect("write planned metadata after-image");
    }
    pager
        .apply_l6_barrier()
        .expect("persist metadata after-images");
    fs::write(&image, &pager.image().bytes).expect("persist planned image");

    docker_e2fsck(&image_name, &fixture, &image);
}

#[test]
fn docker_e2fsck_accepts_depth_two_fragmented_truncate_after_images() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 verification; image unavailable: {image_name}");
        return;
    }

    let fixture = Fixture::docker_mountable();
    let image = fixture.path("depth-two-fragmented-truncate.ext4");
    docker_build_depth_two_fragmented_fixture(&image_name, &fixture, &image);
    let retained_mapping_before = docker_debugfs_bmap(&image_name, &fixture, &image, "/file", 0);

    let mut pager = Ext4Pager::open(VecImage::open(&image)).expect("open Docker ext4 image");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Docker fixture")
        .expect("/file exists");
    let (_, root) = pager
        .inode_meta_and_extent_root(inode)
        .expect("read fragmented extent root");
    assert_eq!(
        ExtentHeader::parse(&root)
            .expect("parse fragmented extent root")
            .depth,
        2,
        "debugfs fixture must force a depth-two extent tree"
    );

    let page = [0xE7; BLOCK_SIZE];
    for logical_block in (2..=2_800u64).step_by(2) {
        let conversion = pager
            .plan_write_page(inode, logical_block, &page, FsyncStamp::new(53))
            .expect("convert one depth-two unwritten extent block");
        assert!(conversion.allocations.is_empty());
        for data in &conversion.data {
            pager
                .apply_l6_write_page(data.physical_block * 8, 8, &data.bytes)
                .expect("write converted data block");
        }
        pager
            .apply_l6_barrier()
            .expect("order converted data before metadata");
        for metadata in &conversion.metadata {
            pager
                .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
                .expect("write converted extent metadata");
        }
        pager
            .apply_l6_barrier()
            .expect("persist converted extent metadata");
    }

    let plan = pager
        .plan_truncate_size(inode, BLOCK_SIZE as u64, FsyncStamp::new(54))
        .expect("plan depth-two truncate");
    assert!(plan.data.is_empty());
    assert!(plan.revokes.len() >= 1_400);
    assert_eq!(plan.revokes.len(), plan.deferred_frees.len());

    for metadata in &plan.metadata {
        pager
            .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
            .expect("write planned metadata after-image");
    }
    pager
        .apply_l6_barrier()
        .expect("persist metadata after-images");
    fs::write(&image, &pager.image().bytes).expect("persist planned image");

    docker_e2fsck(&image_name, &fixture, &image);
    assert_eq!(
        docker_debugfs_bmap(&image_name, &fixture, &image, "/file", 0),
        retained_mapping_before,
        "truncate must retain the logical block zero mapping"
    );
}

#[test]
fn docker_e2fsck_accepts_depth_two_fragmented_unlink_destroy_after_images() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 verification; image unavailable: {image_name}");
        return;
    }

    let fixture = Fixture::docker_mountable();
    let image = fixture.path("depth-two-fragmented-destroy.ext4");
    docker_build_depth_two_fragmented_fixture(&image_name, &fixture, &image);

    let mut pager = Ext4Pager::open(VecImage::open(&image)).expect("open Docker ext4 image");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Docker fixture")
        .expect("/file exists");
    let (_, root) = pager
        .inode_meta_and_extent_root(inode)
        .expect("read fragmented extent root");
    assert_eq!(
        ExtentHeader::parse(&root)
            .expect("parse fragmented extent root")
            .depth,
        2,
        "debugfs fixture must force a depth-two extent tree"
    );

    let page = [0xD5; BLOCK_SIZE];
    for logical_block in (2..=2_800u64).step_by(2) {
        let conversion = pager
            .plan_write_page(inode, logical_block, &page, FsyncStamp::new(55))
            .expect("convert one depth-two unwritten extent block");
        assert!(conversion.allocations.is_empty());
        for data in &conversion.data {
            pager
                .apply_l6_write_page(data.physical_block * 8, 8, &data.bytes)
                .expect("write converted data block");
        }
        pager
            .apply_l6_barrier()
            .expect("order converted data before metadata");
        for metadata in &conversion.metadata {
            pager
                .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
                .expect("write converted extent metadata");
        }
        pager
            .apply_l6_barrier()
            .expect("persist converted extent metadata");
    }

    let unlink = pager
        .plan_unlink_dir_entry(InodeNo::new(2), b"file", inode, FsyncStamp::new(56))
        .expect("plan unlink before destroy");
    for metadata in &unlink.metadata {
        pager
            .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
            .expect("write unlink metadata after-image");
    }
    pager
        .apply_l6_barrier()
        .expect("persist unlink metadata after-images");

    let destroy = pager
        .plan_destroy_inode(inode, FsyncStamp::new(57))
        .expect("plan depth-two destroy");
    assert!(destroy.data.is_empty());
    assert!(destroy.revokes.len() >= 1_400);
    assert_eq!(destroy.revokes.len(), destroy.deferred_frees.len());
    let destroy_superblock = destroy
        .metadata
        .iter()
        .find(|metadata| metadata.home == 0)
        .expect("destroy plan superblock after-image");
    assert_eq!(
        Superblock::parse(&destroy_superblock.after[1024..2048])
            .expect("parse destroy superblock after-image")
            .last_orphan,
        0,
        "destroy must unlink the zero-link inode from the orphan chain"
    );
    for metadata in &destroy.metadata {
        pager
            .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
            .expect("write destroy metadata after-image");
    }
    pager
        .apply_l6_barrier()
        .expect("persist destroy metadata after-images");
    fs::write(&image, &pager.image().bytes).expect("persist planned image");

    docker_e2fsck(&image_name, &fixture, &image);
}

#[test]
#[ignore = "builds a 3 GiB Linux depth-three extent fixture"]
fn docker_e2fsck_accepts_depth_three_fragmented_unwritten_parent_carry_after_images() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 verification; image unavailable: {image_name}");
        return;
    }

    let fixture = Fixture::docker_mountable();
    let image = fixture.path("depth-three-fragmented-parent-carry.ext4");
    docker_build_depth_three_fragmented_fixture(&image_name, &fixture, &image);
    let retained_mapping_before = docker_debugfs_bmap(&image_name, &fixture, &image, "/file", 0);

    let mut pager = Ext4Pager::open(FileImage::open(&image)).expect("open Docker ext4 image");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Docker fixture")
        .expect("/file exists");
    let (_, root) = pager
        .inode_meta_and_extent_root(inode)
        .expect("read fragmented extent root");
    assert_eq!(
        ExtentHeader::parse(&root)
            .expect("parse fragmented extent root")
            .depth,
        3,
        "debugfs fixture must force a depth-three extent tree"
    );

    let target_logical_block =
        find_near_full_unwritten_extent_with_full_parent(&FileImage::open(&image), &root)
            .expect("Linux depth-three fixture must retain a near-full leaf beneath a full parent");
    let conversion = pager
        .plan_write_page(
            inode,
            target_logical_block,
            &[0xE8; BLOCK_SIZE],
            FsyncStamp::new(58),
        )
        .expect("convert one Linux depth-three unwritten extent");
    assert_eq!(conversion.allocations.len(), 2);
    assert_eq!(
        conversion
            .metadata
            .iter()
            .filter(|metadata| metadata.role == tx_ext4_format::mutation::MetaRole::ExtentNode)
            .count(),
        5
    );
    for data in &conversion.data {
        pager
            .apply_l6_write_page(data.physical_block * 8, 8, &data.bytes)
            .expect("write converted data block");
    }
    pager
        .apply_l6_barrier()
        .expect("order converted data before metadata");
    for metadata in &conversion.metadata {
        pager
            .apply_l6_write_page(metadata.home * 8, 8, &metadata.after)
            .expect("write Linux deep-shape metadata after-image");
    }
    pager
        .apply_l6_barrier()
        .expect("persist Linux deep-shape metadata after-images");

    docker_e2fsck(&image_name, &fixture, &image);
    assert_eq!(
        docker_debugfs_bmap(&image_name, &fixture, &image, "/file", 0),
        retained_mapping_before,
        "deep conversion must retain the logical block zero mapping"
    );
}

fn docker_image_available(image_name: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image_name])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_build_inline_root_fixture(image_name: &str, fixture: &Fixture, image: &Path) {
    let mount = format!(
        "type=bind,source={},target=/fixture",
        fixture.root.display()
    );
    let script = format!(
        "set -eu; image=/fixture/{}; dd if=/dev/zero of=\"$image\" bs=1M count=32 status=none; mke2fs -q -t ext4 -F -b 4096 -g 1024 -O extent,^64bit,^metadata_csum \"$image\"; debugfs -w -R 'write /etc/hostname /file' \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 2 2' \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 4 4' \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 6 6' \"$image\" >/dev/null; e2fsck -fn \"$image\" >/dev/null",
        image.file_name().unwrap().to_str().unwrap(),
    );
    run(
        "docker",
        [
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "sh".to_owned(),
            "-lc".to_owned(),
            script,
        ],
    );
}

fn docker_build_depth_two_fragmented_fixture(image_name: &str, fixture: &Fixture, image: &Path) {
    let mount = format!(
        "type=bind,source={},target=/fixture",
        fixture.root.display()
    );
    let script = format!(
        "set -eu; image=/fixture/{}; commands=/fixture/depth-two.debugfs; dd if=/dev/zero of=\"$image\" bs=1M count=64 status=none; mke2fs -q -t ext4 -F -b 4096 -g 1024 -O extent,^64bit,^metadata_csum \"$image\"; printf 'write /etc/hostname /file\\n' > \"$commands\"; for i in $(seq 2 2 2800); do printf 'fallocate /file %s %s\\n' \"$i\" \"$i\" >> \"$commands\"; done; printf 'sif /file size 11472896\\n' >> \"$commands\"; debugfs -w -f \"$commands\" \"$image\" >/dev/null; e2fsck -fn \"$image\" >/dev/null",
        image.file_name().unwrap().to_str().unwrap(),
    );
    run(
        "docker",
        [
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "sh".to_owned(),
            "-lc".to_owned(),
            script,
        ],
    );
}

fn docker_build_depth_three_fragmented_fixture(image_name: &str, fixture: &Fixture, image: &Path) {
    const NODE_MAX: u64 = 340;
    const ROOT_MAX: u64 = 4;
    let unwritten_extent_count = ROOT_MAX * NODE_MAX * NODE_MAX + 1;
    let mount = format!(
        "type=bind,source={},target=/fixture",
        fixture.root.display()
    );
    let script = format!(
        "set -eu; image=/fixture/{}; commands=/fixture/depth-three.debugfs; truncate -s 3G \"$image\"; mke2fs -q -t ext4 -F -b 4096 -O extent,^64bit,^metadata_csum \"$image\"; printf 'write /etc/hostname /file\\n' > \"$commands\"; count={unwritten_extent_count}; i=2; n=0; while [ \"$n\" -lt \"$count\" ]; do start=$i; if [ $((n % 1000)) -eq 999 ]; then end=$((i + 2)); i=$((i + 4)); else end=$i; i=$((i + 2)); fi; printf 'fallocate /file %s %s\\n' \"$start\" \"$end\" >> \"$commands\"; n=$((n + 1)); done; printf 'sif /file size %s\\n' \"$((i * 4096))\" >> \"$commands\"; debugfs -w -f \"$commands\" \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 1 1' \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 3 3' \"$image\" >/dev/null; e2fsck -fn \"$image\" >/dev/null",
        image.file_name().unwrap().to_str().unwrap(),
    );
    run(
        "docker",
        [
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "sh".to_owned(),
            "-lc".to_owned(),
            script,
        ],
    );
}

fn docker_e2fsck(image_name: &str, fixture: &Fixture, image: &Path) {
    let mount = format!(
        "type=bind,source={},target=/fixture,readonly",
        fixture.root.display()
    );
    let image_path = format!("/fixture/{}", image.file_name().unwrap().to_str().unwrap());
    run(
        "docker",
        [
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "e2fsck".to_owned(),
            "-fn".to_owned(),
            image_path,
        ],
    );
}

fn docker_debugfs_bmap(
    image_name: &str,
    fixture: &Fixture,
    image: &Path,
    path: &str,
    logical_block: u64,
) -> String {
    let mount = format!(
        "type=bind,source={},target=/fixture,readonly",
        fixture.root.display()
    );
    let image_path = format!("/fixture/{}", image.file_name().unwrap().to_str().unwrap());
    let command = format!("bmap {path} {logical_block}");
    let output = run(
        "docker",
        [
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "debugfs".to_owned(),
            "-R".to_owned(),
            command,
            image_path,
        ],
    );
    String::from_utf8(output.stdout)
        .expect("debugfs bmap output utf8")
        .trim()
        .to_owned()
}

fn find_near_full_unwritten_extent_with_full_parent(
    image: &FileImage,
    node_bytes: &[u8],
) -> Option<u64> {
    let header = ExtentHeader::parse(node_bytes).ok()?;
    match ExtentNode::parse(node_bytes).ok()? {
        ExtentNode::Leaf(extents) if header.entries + 2 > header.max => extents
            .iter()
            .enumerate()
            .find(|(_, extent)| !extent.is_initialized() && extent.initialized_len() >= 3)
            .map(|(_, extent)| u64::from(extent.logical_block) + 1),
        ExtentNode::Leaf(_) => None,
        ExtentNode::Index(indexes) => {
            let parent_is_full = header.entries == header.max;
            for index in indexes {
                let mut child = [0u8; BLOCK_SIZE];
                image.read_block(index.child, &mut child).ok()?;
                if parent_is_full {
                    if let Some(logical_block) = find_near_full_unwritten_extent(image, &child) {
                        return Some(logical_block);
                    }
                }
                if let Some(logical_block) =
                    find_near_full_unwritten_extent_with_full_parent(image, &child)
                {
                    return Some(logical_block);
                }
            }
            None
        }
    }
}

fn find_near_full_unwritten_extent(image: &FileImage, node_bytes: &[u8]) -> Option<u64> {
    let header = ExtentHeader::parse(node_bytes).ok()?;
    match ExtentNode::parse(node_bytes).ok()? {
        ExtentNode::Leaf(extents) if header.entries + 2 > header.max => extents
            .iter()
            .find(|extent| !extent.is_initialized() && extent.initialized_len() >= 3)
            .map(|extent| u64::from(extent.logical_block) + 1),
        ExtentNode::Leaf(_) => None,
        ExtentNode::Index(indexes) => {
            for index in indexes {
                let mut child = [0u8; BLOCK_SIZE];
                image.read_block(index.child, &mut child).ok()?;
                if let Some(logical_block) = find_near_full_unwritten_extent(image, &child) {
                    return Some(logical_block);
                }
            }
            None
        }
    }
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
        .filter(|tool| command_or_candidates(tool).is_none())
        .map(str::to_owned)
        .collect()
}

fn command_or_candidates(tool: &str) -> Option<String> {
    if command_exists(tool) {
        return Some(tool.to_string());
    }
    let candidates: &[&str] = match tool {
        "mkfs.ext4" => &[
            "/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext4",
            "/opt/homebrew/sbin/mkfs.ext4",
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4",
            "/usr/local/opt/e2fsprogs/sbin/mkfs.ext4",
            "/usr/local/sbin/mkfs.ext4",
        ],
        "e2fsck" => &[
            "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
            "/opt/homebrew/sbin/e2fsck",
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/e2fsck",
            "/usr/local/opt/e2fsprogs/sbin/e2fsck",
            "/usr/local/sbin/e2fsck",
        ],
        _ => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|candidate| command_exists(candidate))
        .map(str::to_string)
}

fn command_exists(tool: &str) -> bool {
    if Path::new(tool).is_file() {
        return true;
    }
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

    fn docker_mountable() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join(format!(
                "tx-ext4-format-docker-{}-{unique}",
                std::process::id()
            ));
        fs::create_dir_all(&root).expect("create Docker-mountable fixture temp dir");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
