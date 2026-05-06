//! Tests for `exec_script` (Phase 5 of the ELF-loader plan).
//!
//! These tests drive the full eight-phase EXEC_v1 protocol against a
//! kernel-resident init process. The fixture wires up a tmpfs-shaped
//! `FsOps` whose `materialise_rnode` produces `RNodeBacking::PageBacked`
//! over a hand-crafted minimal RV64 ET_EXEC ELF binary, so the walker
//! resolves `path` → `Cap<OpenFile>` → `Cap<PageContainer>` end-to-end
//! and the script's parse + AS-build + stack-populate + Phase-6 swap
//! lanes all run.
//!
//! Doc anchors: same as `script.rs`'s. The tests exercise:
//! - Phase 6 atomic AS-replacement (`exec_script_loads_minimal_elf_seeds_saved_user_context`).
//! - `EXEC-12-4-INSTALL-BRK` (`exec_script_resets_brk_base_from_image_plan`).
//! - `EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`.
//! - `EXEC-12-2-RESET-FDS-WITH-CLOEXEC`.
//! - Pre-PoNR error rollback for `ENOENT` and malformed ELF.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot,
    PmapUnmapResult, PtNode, VirtAddr,
};
use tx_substrate::page_allocator;
use tx_substrate::zone::{self, Cap};
use tx_substrate::SpinMutex;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::execution::{Errno, Guard, StepOutcome};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::{
    AnonSwapPolicy, Frame, FsPageBacking, MaterializeAccess, PageContainer, PageContainerKind,
    PageIndex,
};
use tx_subsystems::process::{bootstrap_init_process, step_chdir, ChdirOutcome, ProcessIdentity};
use tx_subsystems::signal::{SigDisposition, Signum};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::{
    Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking, S_IFDIR, S_IFREG,
};
use tx_subsystems::vfs::FsOps;
use tx_subsystems::vm::{AddressSpace, USER_PAGE_SIZE};
use tx_subsystems::zones;

use super::{exec_script, ExecError};

// ---------------------------------------------------------------------------
// Test platform — minimal `PmapIf` that satisfies `AddressSpace::new`.
//
// Mirrors the in-crate `tx-shims` test pmap. Simulates pmap-root
// alloc/dealloc, reserve/commit/unmap, and tracks installed mappings
// keyed by `(root, virt)`. The exec script only reads / publishes
// pmap entries via the standard `vm::scripts` lane; the test pmap is
// expressive enough to drive that path end-to-end.
// ---------------------------------------------------------------------------

struct ScriptsTestPmap;

#[derive(Default)]
struct ScriptsTestPmapState {
    next_root: usize,
    mappings: BTreeMap<(usize, usize), PhysAddr>,
}

static SCRIPTS_TEST_PMAP_STATE: LazyLock<Mutex<ScriptsTestPmapState>> = LazyLock::new(|| {
    Mutex::new(ScriptsTestPmapState {
        next_root: 1,
        mappings: BTreeMap::new(),
    })
});

fn root_key(root: &PmapRoot) -> usize {
    root.phys().0
}

impl PmapIf for ScriptsTestPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        let id = state.next_root;
        state.next_root += 1;
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * USER_PAGE_SIZE)),
            Asid(id as u16),
        ))
    }

    fn destroy_pmap_root(root: PmapRoot) {
        let mut state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        let key = root.phys().0;
        state.mappings.retain(|(r, _), _| *r != key);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        let state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        if state.mappings.contains_key(&(root_key(root), virt.0)) {
            return Err(PmapError::AlreadyMapped);
        }
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(
        root: &PmapRoot,
        reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
        let mut state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        state
            .mappings
            .insert((root_key(root), reservation.virt().0), reservation.phys());
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        let mut state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        let Some(phys) = state.mappings.remove(&(root_key(root), virt.0)) else {
            return Ok(None);
        };
        Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
    }
}

// ---------------------------------------------------------------------------
// Minimal in-test FS that knows how to materialise regular files as
// `RNodeBacking::PageBacked { pc }` over a kernel-built PageContainer.
//
// The walker tests (in `tx-subsystems`) cover directories/symlinks; the
// exec script needs the regular-file path that tmpfs's production
// surface will eventually plug in via a `materialise_rnode` override.
// We build the same shape here as a self-contained fixture.
// ---------------------------------------------------------------------------

struct ExecTestFs {
    inner: SpinMutex<ExecTestFsInner>,
}

struct ExecTestFsInner {
    children: BTreeMap<FsObjectId, BTreeMap<Vec<u8>, FsObjectId>>,
    inodes: BTreeMap<FsObjectId, ExecTestInode>,
    next_id: u64,
}

enum ExecTestInode {
    Directory,
    Regular {
        container: Cap<PageContainer>,
        size: u64,
    },
}

impl ExecTestFs {
    fn new(root_id: FsObjectId) -> Arc<Self> {
        let mut inner = ExecTestFsInner {
            children: BTreeMap::new(),
            inodes: BTreeMap::new(),
            next_id: root_id.as_u64() + 1,
        };
        inner.children.insert(root_id, BTreeMap::new());
        inner.inodes.insert(root_id, ExecTestInode::Directory);
        Arc::new(Self {
            inner: SpinMutex::new(inner),
        })
    }

    fn alloc_id(&self) -> FsObjectId {
        let mut inner = self.inner.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        FsObjectId::new(id)
    }

    /// Create a regular-file inode whose page-backed contents are
    /// pre-populated with `bytes`. Returns the file's FsObjectId.
    fn add_regular_with_bytes(&self, parent: FsObjectId, name: &[u8], bytes: &[u8]) -> FsObjectId {
        let pages = ((bytes.len() as u64) + USER_PAGE_SIZE as u64 - 1) / USER_PAGE_SIZE as u64;
        let pages = core::cmp::max(pages, 1);
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            pages,
        )
        .expect("page container reservation");
        // Populate every page with the corresponding byte slice via
        // `materialize_anon` + the kernel direct map.
        for (idx, chunk) in bytes.chunks(USER_PAGE_SIZE).enumerate() {
            let materialised = pc
                .materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write)
                .expect("materialize anon page for fixture");
            let frame_base =
                page_allocator::frame_kernel_addr(materialised.ppn).expect("direct-map view");
            // SAFETY: freshly materialised anon page; we hold the pin
            // via `materialised.map_pin` for the duration of the copy.
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
            }
        }
        let size = bytes.len() as u64;
        // Truncate updates `pc.size_bytes()` to match POSIX semantics;
        // we set it directly via a guard-scoped `step_truncate` rather
        // than reaching into `set_size_bytes` (pub(crate)).
        let guard = tx_substrate::epoch::guard();
        match tx_subsystems::page_backed::step_truncate(&pc, size, &guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {}
            other => panic!("step_truncate(pc, {size}) failed: {other:?}"),
        }
        drop(guard);

        let id = self.alloc_id();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.inodes.insert(
            id,
            ExecTestInode::Regular {
                container: pc,
                size,
            },
        );
        id
    }
}

impl FsOps for ExecTestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        match map.get(name) {
            Some(id) => StepOutcome::Done(*id),
            None => StepOutcome::Err(Errno::ENOENT),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let meta = match inode {
            ExecTestInode::Directory => InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            ExecTestInode::Regular { size, .. } => {
                let mut meta = InodeMeta::new(InodeKind::Regular, S_IFREG | 0o755);
                meta.size = *size;
                meta
            }
        };
        StepOutcome::Done(meta)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Done(None)
    }

    fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn read_link(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<Box<[u8]>> {
        StepOutcome::Err(Errno::EINVAL)
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        match inode {
            ExecTestInode::Regular { container, .. } => {
                match RNode::new_cap(
                    fs_object_id,
                    meta,
                    RNodeBacking::PageBacked {
                        pc: container.clone(),
                    },
                ) {
                    Ok(rnode) => StepOutcome::Done(rnode),
                    Err(_) => StepOutcome::Err(Errno::ENOMEM),
                }
            }
            ExecTestInode::Directory => StepOutcome::Err(Errno::EISDIR),
        }
    }
}

impl FsPageBacking for ExecTestFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame> {
        let inner = self.inner.lock();
        let container = match inner.inodes.get(&fs_object_id) {
            Some(ExecTestInode::Regular { container, .. }) => container.clone(),
            Some(ExecTestInode::Directory) => return StepOutcome::Err(Errno::EISDIR),
            None => return StepOutcome::Err(Errno::ENOENT),
        };
        drop(inner);

        let page_size = USER_PAGE_SIZE as u64;
        if offset % page_size != 0 {
            return StepOutcome::Err(Errno::EINVAL);
        }
        let page_index = PageIndex::new(offset / page_size);
        match container.materialize_page(page_index, MaterializeAccess::Read, guard) {
            StepOutcome::Done(materialised) => StepOutcome::Done(Frame::new(materialised.ppn)),
            StepOutcome::Advanced(materialised) => {
                StepOutcome::Advanced(Frame::new(materialised.ppn))
            }
            StepOutcome::AdvancedThenBlocked(materialised, token) => {
                StepOutcome::AdvancedThenBlocked(Frame::new(materialised.ppn), token)
            }
            StepOutcome::Blocked(token) => StepOutcome::Blocked(token),
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
        }
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
    }
}

// ---------------------------------------------------------------------------
// ELF fixture builder. Produces a minimal RV64 ET_EXEC binary the
// parser accepts. Mirrors `loader/tests.rs`'s shape but inline-includes
// only what `exec_script` needs.
// ---------------------------------------------------------------------------

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;
const PF_R: u32 = 4;
const PF_X: u32 = 1;

const FIX_PAGE: u64 = 4096;
const BASE_LOAD_VADDR: u64 = 0x1_0000;
const ENTRY_OFFSET: u64 = 0x80;

/// Emit a minimal valid RV64 ET_EXEC binary with one R+X LOAD covering
/// the file (no BSS extension). The file fits in a single page so
/// the exec script's 4 KiB initial read covers the entire image.
fn minimal_elf_bytes() -> Vec<u8> {
    fn write_u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn write_phdr(
        b: &mut [u8],
        at: usize,
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    ) {
        write_u32(b, at, p_type);
        write_u32(b, at + 4, p_flags);
        write_u64(b, at + 8, p_offset);
        write_u64(b, at + 16, p_vaddr);
        write_u64(b, at + 24, p_vaddr); // paddr
        write_u64(b, at + 32, p_filesz);
        write_u64(b, at + 40, p_memsz);
        write_u64(b, at + 48, p_align);
    }

    // Layout: header (64) + PT_PHDR (56) + PT_LOAD (56) = 176 bytes.
    let phoff: u64 = 64;
    let n_phdrs: u16 = 2;
    let phent: u16 = 56;
    let total_phdrs = (n_phdrs as u64) * (phent as u64);
    let file_size: usize = (phoff + total_phdrs) as usize;
    let mut bytes = vec![0u8; file_size];

    // ----- Ehdr -----
    bytes[0..4].copy_from_slice(&ELF_MAGIC);
    bytes[4] = ELFCLASS64;
    bytes[5] = ELFDATA2LSB;
    bytes[6] = EV_CURRENT;
    write_u16(&mut bytes, 16, ET_EXEC);
    write_u16(&mut bytes, 18, EM_RISCV);
    write_u32(&mut bytes, 20, 1); // e_version
    write_u64(&mut bytes, 24, BASE_LOAD_VADDR + ENTRY_OFFSET); // e_entry
    write_u64(&mut bytes, 32, phoff); // e_phoff
    write_u64(&mut bytes, 40, 0); // e_shoff
    write_u32(&mut bytes, 48, 0); // e_flags
    write_u16(&mut bytes, 52, 64); // e_ehsize
    write_u16(&mut bytes, 54, phent); // e_phentsize
    write_u16(&mut bytes, 56, n_phdrs); // e_phnum
    write_u16(&mut bytes, 58, 0); // e_shentsize
    write_u16(&mut bytes, 60, 0); // e_shnum
    write_u16(&mut bytes, 62, 0); // e_shstrndx

    // ----- PT_PHDR -----
    let pt_phdr_vaddr = BASE_LOAD_VADDR + phoff;
    write_phdr(
        &mut bytes,
        phoff as usize,
        PT_PHDR,
        PF_R,
        phoff,
        pt_phdr_vaddr,
        total_phdrs,
        total_phdrs,
        8,
    );

    // ----- PT_LOAD (R+X) -----
    write_phdr(
        &mut bytes,
        (phoff + 56) as usize,
        PT_LOAD,
        PF_R | PF_X,
        0,
        BASE_LOAD_VADDR,
        file_size as u64,
        file_size as u64,
        FIX_PAGE,
    );

    bytes
}

// ---------------------------------------------------------------------------
// Test infrastructure: shared lock + bootstrap fixture.
// ---------------------------------------------------------------------------

static SCRIPT_TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestSetup {
    _lock: MutexGuard<'static, ()>,
}

fn setup() -> TestSetup {
    let lock = SCRIPT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    let _ = zones::register_all();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for exec_script tests: {error:?}"),
    }
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    TestSetup { _lock: lock }
}

/// Register a fresh `ExecTestFs` rootfs and return (root_dentry, fs).
fn build_fs_root() -> (Cap<DEntry>, Arc<ExecTestFs>) {
    let root_id = FsObjectId::new(2);
    let fs = ExecTestFs::new(root_id);

    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn FsOps>,
        fs.clone() as Arc<dyn FsPageBacking>,
        None,
        DevId::new(99),
        MountOptions::default(),
        "exec-test-fs",
        SourceLabel::Static("exec-test"),
    )
    .expect("mount payload");

    let root_rnode = {
        let raw = RNode::new(
            root_id,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = zone::reserve_for::<RNode>().expect("rnode reservation");
        zone::sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(1),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    (root_dentry, fs)
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<ScriptsTestPmap>().expect("fresh aspace")
}

fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll, Waker};
    use std::sync::Arc;

    struct NoopWake;
    impl std::task::Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }

    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut pinned = Box::pin(future);
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("exec_script tests block_on: future did not resolve in 1024 polls");
}

/// Bootstrap an init process whose cwd is the registered fs root, and
/// register a regular file at `/<name>` containing `bytes`. Returns
/// the leader thread, the process Cap, and the fs handle (for further
/// fixture mutation if needed).
fn bootstrap_with_file(
    name: &[u8],
    bytes: &[u8],
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecTestFs>) {
    let (root_dentry, fs) = build_fs_root();
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), name, bytes);

    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");

    // Bind cwd so the walker has a search root.
    match step_chdir(&process, root_dentry) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }

    (process, thread, fs)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn exec_script_loads_minimal_elf_seeds_saved_user_context() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::default();

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    let aspace_after = process.aspace_cap().expect("alive aspace post-exec");
    assert_ne!(
        aspace_before.key(),
        aspace_after.key(),
        "Phase 6 must have atomically replaced the aspace"
    );

    let payload = thread.payload_cap().expect("alive thread payload");
    let ctx = payload
        .saved_user_context()
        .expect("Phase 6 must have seeded the trap context");
    assert_eq!(
        ctx.pc as u64,
        BASE_LOAD_VADDR + ENTRY_OFFSET,
        "saved_user_context.pc should match the ELF entry"
    );
    // Per RV64 psABI the SP lives at register x2.
    assert_eq!(ctx.regs[2] & 0xF, 0, "initial sp must be 16-byte aligned");
    assert_ne!(ctx.regs[2], 0, "initial sp must be populated");
    // Other GPRs (besides x2) are zero per System V `_start` contract.
    assert_eq!(ctx.regs[0], 0);
    assert_eq!(ctx.regs[1], 0);
}

#[test]
fn exec_script_resets_brk_base_from_image_plan() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let cred = Credential::default();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    // brk_base should be page_round_up(BASE_LOAD_VADDR + memsz). The
    // fixture's memsz == 176, so highest = 0x1_0000 + 176, rounded up
    // to the next 4 KiB page = 0x1_1000.
    let expected = (BASE_LOAD_VADDR + 176 + 4095) & !4095;
    assert_eq!(process.brk_base(), expected);
    assert_eq!(process.current_brk(), expected);
}

#[test]
fn exec_script_invalid_elf_returns_not_executable() {
    let _setup = setup();
    // 4 KiB of zeroes — fails ELF magic check immediately.
    let bytes = vec![0u8; 4096];
    let (process, thread, _fs) = bootstrap_with_file(b"bad", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::default();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/bad",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Err(ExecError::NotExecutable));

    // Pre-PoNR error path must leave the process aspace untouched.
    let aspace_after = process.aspace_cap().expect("alive aspace post-fail");
    assert_eq!(aspace_before.key(), aspace_after.key());
    // The thread's saved_user_context must remain empty (bootstrap
    // never seeds it).
    let payload = thread.payload_cap().expect("alive thread payload");
    assert!(payload.saved_user_context().is_none());
}

#[test]
fn exec_script_path_not_found_returns_path_not_found() {
    let _setup = setup();
    // Register a file but exec a different path so the walker
    // returns ENOENT before any read or parse.
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::default();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/nope",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Err(ExecError::PathNotFound));

    let aspace_after = process.aspace_cap().expect("alive aspace post-fail");
    assert_eq!(aspace_before.key(), aspace_after.key());
}

#[test]
fn exec_script_resets_signal_dispositions_to_sig_dfl() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    // Pre-install a non-default disposition for SIGTERM via the
    // existing step_sigaction step; verify it reads back as Handler
    // before the exec.
    use tx_subsystems::signal::step_sigaction;
    let _ = step_sigaction(&process, Signum::SIGTERM, SigDisposition::Handler(0xdead));
    assert_eq!(
        process
            .sig_disposition(Signum::SIGTERM)
            .expect("SIGTERM disposition pre-exec"),
        SigDisposition::Handler(0xdead),
    );

    let cred = Credential::default();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    // After exec, the handler must have been collapsed back to
    // SigDisposition::Default per EXEC-12-3-RESET-SIGNAL-DISPOSITIONS.
    assert_eq!(
        process
            .sig_disposition(Signum::SIGTERM)
            .expect("SIGTERM disposition post-exec"),
        SigDisposition::Default,
    );
}

#[test]
fn exec_script_closes_cloexec_fds_keeps_others() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, fs) = bootstrap_with_file(b"init", &bytes);

    // Open two distinct files at fd 3 (CLOEXEC) and fd 4 (kept). We
    // reuse the binary itself as the file backing for each fd —
    // the test only cares about presence/absence of the slot.
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), b"keep", b"hello-keep-me");

    use tx_subsystems::vfs::structure::{OpenFile, OpenFileFlags};
    // Build OpenFiles directly over the test fs's PageContainers via
    // `materialise_rnode` — tx-subsystems::vfs::OpenFile::new_cap is
    // public.
    let open_for = |name: &[u8]| -> Cap<OpenFile> {
        // Resolve through the walker; it will go through
        // materialise_rnode and produce a PageBacked RNode.
        let cred = Credential::default();
        let cwd = process.cwd().expect("cwd bound");
        let guard = tx_substrate::epoch::guard();
        let outcome = block_on(tx_subsystems::vfs::walker::step_open(
            cwd,
            name,
            OpenFileFlags {
                read: true,
                write: false,
                append: false,
                cloexec: false,
            },
            0,
            &cred,
            &guard,
        ));
        drop(guard);
        match outcome {
            StepOutcome::Done(file) | StepOutcome::Advanced(file) => file,
            other => panic!("step_open({name:?}) failed: {other:?}"),
        }
    };

    let cloexec_file = open_for(b"/init");
    let kept_file = open_for(b"/keep");
    let _ = process.set_fd(3, Some(cloexec_file));
    let _ = process.set_fd(4, Some(kept_file));
    process.set_fd_cloexec(3, true);
    process.set_fd_cloexec(4, false);
    assert!(process.fd(3).is_some());
    assert!(process.fd(4).is_some());

    let cred = Credential::default();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    // fd 3 was CLOEXEC: closed.
    assert!(process.fd(3).is_none(), "CLOEXEC fd must be closed by exec");
    // fd 4 was NOT CLOEXEC: kept.
    assert!(process.fd(4).is_some(), "non-CLOEXEC fd must survive exec");
    // The CLOEXEC bitmap is cleared wholesale.
    assert!(!process.fd_cloexec(3));
    assert!(!process.fd_cloexec(4));
}
