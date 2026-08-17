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

use crate::adapter::step_engine::{
    self as step_engine, guard, page_allocator, reserve_for, sign_for, Cap, SpinMutex, StepOp,
    StepOutcome,
};
use crate::adapter::vfs_exec::{
    Credential, DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
};
use tx_hal::{
    Arch, Asid, EntropyIf, PhysAddr, PlatformConfig, PmapError, PmapIf, PmapPermissions,
    PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, UserPtr, VirtAddr,
};
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions, MountPayload,
    MountPayloadPin, SourceLabel,
};
use tx_subsystems::page_backed::{
    AnonSwapPolicy, Frame, FsPageBacking, MaterializeAccess, PageContainer, PageContainerKind,
    PageIndex,
};
use tx_subsystems::process::{
    bootstrap_init_process, step_chdir, step_chdir_with_mount, step_set_mount_namespace,
    ChdirOutcome, ProcessIdentity,
};
use tx_subsystems::signal::{SigDisposition, Signum};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::{AddressSpace, UserVirtAddr, USER_PAGE_SIZE};
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

struct ScriptsLa64TestPmap;

impl PlatformConfig for ScriptsTestPmap {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "scripts-test";
    const USER_TOP: VirtAddr = VirtAddr(0x8000_0000);
}

impl PlatformConfig for ScriptsLa64TestPmap {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "scripts-la64-test";
    const USER_TOP: VirtAddr = VirtAddr(0x8000_0000);
}

#[derive(Default)]
struct ScriptsTestPmapState {
    next_root: usize,
    mappings: BTreeMap<(usize, usize), PhysAddr>,
    fail_next_create_root: bool,
}

static SCRIPTS_TEST_PMAP_STATE: LazyLock<Mutex<ScriptsTestPmapState>> = LazyLock::new(|| {
    Mutex::new(ScriptsTestPmapState {
        next_root: 1,
        mappings: BTreeMap::new(),
        fail_next_create_root: false,
    })
});

fn fail_next_pmap_root_creation() {
    SCRIPTS_TEST_PMAP_STATE
        .lock()
        .expect("scripts pmap lock")
        .fail_next_create_root = true;
}

fn root_key(root: &PmapRoot) -> usize {
    root.phys().0
}

impl PmapIf for ScriptsTestPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let mut state = SCRIPTS_TEST_PMAP_STATE.lock().expect("scripts pmap lock");
        if core::mem::take(&mut state.fail_next_create_root) {
            return Err(PmapError::Exhausted);
        }
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

impl PmapIf for ScriptsLa64TestPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        ScriptsTestPmap::create_pmap_root()
    }

    fn destroy_pmap_root(root: PmapRoot) {
        ScriptsTestPmap::destroy_pmap_root(root);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        ScriptsTestPmap::reserve_mapping(root, virt, phys, kind)
    }

    fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
        ScriptsTestPmap::rollback_mapping(root, reservation);
    }

    fn commit_mapping(root: &PmapRoot, reservation: PmapReservation, permissions: PmapPermissions) {
        ScriptsTestPmap::commit_mapping(root, reservation, permissions);
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        ScriptsTestPmap::unmap_mapping(root, virt, kind)
    }
}

impl tx_hal::ConsoleIf for ScriptsTestPmap {
    fn write_bytes(_bytes: &[u8]) {}
}

// `exec_script::<P>` requires `P: PmapIf + EntropyIf`. The trait
// default fills bytes from the deterministic boot-counter
// xorshift, which is exactly what test sites want — non-zero,
// reproducible, no hardware dependency.
impl EntropyIf for ScriptsTestPmap {}

impl tx_hal::AuxvIf for ScriptsTestPmap {}

impl tx_hal::ConsoleIf for ScriptsLa64TestPmap {
    fn write_bytes(_bytes: &[u8]) {}
}

impl EntropyIf for ScriptsLa64TestPmap {}

impl tx_hal::AuxvIf for ScriptsLa64TestPmap {}

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
    Symlink {
        target: Vec<u8>,
    },
    RegularNonPageBacked {
        mode_bits: u16,
        uid: u32,
        gid: u32,
    },
    Regular {
        container: Cap<PageContainer>,
        size: u64,
        /// Mode bits (without S_IFMT). Wave 4 Part 5 tests vary this
        /// to exercise the X-bit and S_ISUID/S_ISGID paths.
        mode_bits: u16,
        /// Owner uid stored on the inode meta. Lets tests register a
        /// setuid binary owned by uid 1000 distinct from the caller.
        uid: u32,
        /// Owner gid stored on the inode meta.
        gid: u32,
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

    fn add_directory(&self, parent: FsObjectId, name: &[u8]) -> FsObjectId {
        let id = self.alloc_id();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.children.insert(id, BTreeMap::new());
        inner.inodes.insert(id, ExecTestInode::Directory);
        id
    }

    fn add_symlink(&self, parent: FsObjectId, name: &[u8], target: &[u8]) -> FsObjectId {
        let id = self.alloc_id();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.inodes.insert(
            id,
            ExecTestInode::Symlink {
                target: target.to_vec(),
            },
        );
        id
    }

    /// Create a regular-file inode whose page-backed contents are
    /// pre-populated with `bytes`. Returns the file's FsObjectId.
    fn add_regular_with_bytes(&self, parent: FsObjectId, name: &[u8], bytes: &[u8]) -> FsObjectId {
        // Default mode/uid/gid match the pre-Wave-4 fixture: 0o755,
        // root-owned. Wave 4 Part 5 tests use
        // `add_regular_with_bytes_meta` to override.
        self.add_regular_with_bytes_meta(parent, name, bytes, 0o755, 0, 0)
    }

    /// Create a regular-file inode with explicit mode bits (without
    /// `S_IFMT`) and uid/gid. Used by Wave 4 Part 5 tests to drive
    /// the X-bit and `S_ISUID` / `S_ISGID` paths.
    fn add_regular_with_bytes_meta(
        &self,
        parent: FsObjectId,
        name: &[u8],
        bytes: &[u8],
        mode_bits: u16,
        uid: u32,
        gid: u32,
    ) -> FsObjectId {
        let pages = (bytes.len() as u64).div_ceil(USER_PAGE_SIZE as u64);
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
        let guard = guard();
        match tx_subsystems::page_backed::step_truncate(&pc, size, &guard) {
            StepOutcome::Done(()) | StepOutcome::Continue { .. } => {}
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
                mode_bits,
                uid,
                gid,
            },
        );
        id
    }

    fn add_non_page_backed_regular(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode_bits: u16,
        uid: u32,
        gid: u32,
    ) -> FsObjectId {
        let id = self.alloc_id();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.inodes.insert(
            id,
            ExecTestInode::RegularNonPageBacked {
                mode_bits,
                uid,
                gid,
            },
        );
        id
    }

    fn add_regular_with_container(
        &self,
        parent: FsObjectId,
        name: &[u8],
        container: Cap<PageContainer>,
        size: u64,
    ) -> FsObjectId {
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
                container,
                size,
                mode_bits: 0o755,
                uid: 0,
                gid: 0,
            },
        );
        id
    }

    fn set_regular_mode_bits(&self, fs_object_id: FsObjectId, mode_bits: u16) {
        let mut inner = self.inner.lock();
        match inner.inodes.get_mut(&fs_object_id) {
            Some(ExecTestInode::Regular {
                mode_bits: current, ..
            })
            | Some(ExecTestInode::RegularNonPageBacked {
                mode_bits: current, ..
            }) => *current = mode_bits,
            Some(ExecTestInode::Directory | ExecTestInode::Symlink { .. }) => {
                panic!("set_regular_mode_bits called for non-regular inode")
            }
            None => panic!("set_regular_mode_bits called for missing inode"),
        }
    }
}

struct ErrnoPageBacking(tx_subsystems::execution::Errno);

impl FsPageBacking for ErrnoPageBacking {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<Frame, step_engine::NoProgress> {
        step_engine::StepOutcome::err(self.0.into())
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }
}

struct OffsetErrnoPageBacking {
    bytes: Vec<u8>,
    fail_offset: u64,
    errno: tx_subsystems::execution::Errno,
}

impl FsPageBacking for OffsetErrnoPageBacking {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        offset: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<Frame, step_engine::NoProgress> {
        if offset == self.fail_offset {
            return step_engine::StepOutcome::err(self.errno.into());
        }
        let reservation = match tx_subsystems::page_backed::reserve_frame_with_reclaim(
            page_allocator::ZeroPolicy::Zeroed,
        ) {
            Ok(reservation) => reservation,
            Err(_) => {
                return step_engine::StepOutcome::err(tx_subsystems::execution::Errno::EBUSY.into())
            }
        };
        let owned = reservation.commit();
        let start = usize::try_from(offset).expect("test file offset fits usize");
        if start < self.bytes.len() {
            let end = core::cmp::min(start + USER_PAGE_SIZE, self.bytes.len());
            page_allocator::testing::write_frame_bytes_for_test(
                owned.ppn(),
                0,
                &self.bytes[start..end],
            );
        }
        step_engine::StepOutcome::done(Frame::from_owned(owned))
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> step_engine::StepOutcome<(), step_engine::NoProgress> {
        step_engine::StepOutcome::done(())
    }
}

// `FsOps` + `FsPageBacking` impls + tests on `ExecTestFs` live in
// the sibling `v3` submodule (file: `script/tests/v3.rs`). The
// submodule has full visibility into `ExecTestFs` via `super::`.
mod v3;

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
const EM_LOONGARCH: u16 = 258;
const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PT_RISCV_ATTRIBUTES: u32 = 0x7000_0003;
const PF_R: u32 = 4;
const PF_W: u32 = 2;
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
    #[allow(clippy::too_many_arguments)]
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

fn bss_tail_elf_bytes() -> Vec<u8> {
    let mut bytes = minimal_elf_bytes();
    let filesz = FIX_PAGE + 128;
    let memsz = filesz + 128;
    bytes.resize(filesz as usize, 0x5a);
    let load_phdr = 64 + 56;
    bytes[load_phdr + 32..load_phdr + 40].copy_from_slice(&filesz.to_le_bytes());
    bytes[load_phdr + 40..load_phdr + 48].copy_from_slice(&memsz.to_le_bytes());
    bytes
}

/// Emit a minimal RV64 ET_DYN image. `interpreter_path` is encoded as a
/// NUL-terminated PT_INTERP string when present.
fn dynamic_elf_bytes(interpreter_path: Option<&[u8]>) -> Vec<u8> {
    fn write_u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    #[allow(clippy::too_many_arguments)]
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
        write_u64(b, at + 24, p_vaddr);
        write_u64(b, at + 32, p_filesz);
        write_u64(b, at + 40, p_memsz);
        write_u64(b, at + 48, p_align);
    }

    const ET_DYN: u16 = 3;
    const PT_INTERP: u32 = 3;

    let phoff = 64u64;
    let phnum = if interpreter_path.is_some() {
        3u16
    } else {
        2u16
    };
    let phdr_bytes = u64::from(phnum) * 56;
    let interp_offset = phoff + phdr_bytes;
    let interp_len = interpreter_path.map_or(0, |path| path.len() + 1);
    let file_size = interp_offset as usize + interp_len;
    let mut bytes = vec![0u8; file_size];

    bytes[..4].copy_from_slice(&ELF_MAGIC);
    bytes[4] = ELFCLASS64;
    bytes[5] = ELFDATA2LSB;
    bytes[6] = EV_CURRENT;
    write_u16(&mut bytes, 16, ET_DYN);
    write_u16(&mut bytes, 18, EM_RISCV);
    write_u32(&mut bytes, 20, 1);
    write_u64(&mut bytes, 24, ENTRY_OFFSET);
    write_u64(&mut bytes, 32, phoff);
    write_u16(&mut bytes, 52, 64);
    write_u16(&mut bytes, 54, 56);
    write_u16(&mut bytes, 56, phnum);

    write_phdr(
        &mut bytes,
        phoff as usize,
        PT_PHDR,
        PF_R,
        phoff,
        phoff,
        phdr_bytes,
        phdr_bytes,
        8,
    );
    write_phdr(
        &mut bytes,
        phoff as usize + 56,
        PT_LOAD,
        PF_R | PF_X,
        0,
        0,
        file_size as u64,
        file_size as u64,
        FIX_PAGE,
    );
    if let Some(path) = interpreter_path {
        write_phdr(
            &mut bytes,
            phoff as usize + 112,
            PT_INTERP,
            PF_R,
            interp_offset,
            interp_offset,
            interp_len as u64,
            interp_len as u64,
            1,
        );
        let start = interp_offset as usize;
        bytes[start..start + path.len()].copy_from_slice(path);
        bytes[start + path.len()] = 0;
    }
    bytes
}

fn read_initial_auxv(
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
) -> BTreeMap<u64, u64> {
    let context = thread
        .payload_cap()
        .and_then(|payload| payload.saved_user_context())
        .expect("saved user context");
    read_initial_auxv_at_sp(process, context.regs[2] as u64)
}

fn read_initial_auxv_at_sp(process: &Cap<ProcessIdentity>, sp: u64) -> BTreeMap<u64, u64> {
    let aspace = process.aspace_cap().expect("post-exec aspace");
    let stack_entry = aspace
        .lookup(UserVirtAddr(sp as usize))
        .expect("initial stack entry");
    let readable = stack_entry.range.end().as_usize() - sp as usize;
    let mut bytes = vec![0u8; readable.min(1024)];
    let guard = guard();
    match aspace.copy_from_user(&mut bytes, UserPtr::new(sp as usize), &guard) {
        StepOutcome::Done(copied) => assert!(copied >= 128, "short initial stack: {copied}"),
        other => panic!("copy initial stack failed: {other:?}"),
    }
    drop(guard);

    let word = |offset: usize| {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("stack word"))
    };
    let argc = word(0) as usize;
    let mut offset = 8 + (argc + 1) * 8;
    while word(offset) != 0 {
        offset += 8;
    }
    offset += 8;

    let mut auxv = BTreeMap::new();
    loop {
        let kind = word(offset);
        let value = word(offset + 8);
        offset += 16;
        if kind == 0 {
            break;
        }
        auxv.insert(kind, value);
    }
    auxv
}

fn read_user_cstring(process: &Cap<ProcessIdentity>, ptr: u64) -> Vec<u8> {
    let aspace = process.aspace_cap().expect("post-exec aspace");
    let entry = aspace
        .lookup(UserVirtAddr(ptr as usize))
        .expect("cstring mapping");
    let readable = entry.range.end().as_usize() - ptr as usize;
    let mut bytes = vec![0u8; readable.min(4096)];
    let guard = guard();
    let copied = match aspace.copy_from_user(&mut bytes, UserPtr::new(ptr as usize), &guard) {
        StepOutcome::Done(copied) => copied,
        other => panic!("copy user cstring failed: {other:?}"),
    };
    drop(guard);
    let nul = bytes[..copied]
        .iter()
        .position(|byte| *byte == 0)
        .expect("NUL-terminated user string");
    bytes.truncate(nul);
    bytes
}

/// Real freestanding RV64 toolchain shape used by the vDSO guest witness:
/// one RX LOAD, a RISC-V attributes header, and an NX GNU-stack request.
fn static_toolchain_elf_bytes() -> Vec<u8> {
    fn write_u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    #[allow(clippy::too_many_arguments)]
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
        write_u64(b, at + 24, p_vaddr);
        write_u64(b, at + 32, p_filesz);
        write_u64(b, at + 40, p_memsz);
        write_u64(b, at + 48, p_align);
    }

    let mut bytes = vec![0u8; 0x102e];
    bytes[..4].copy_from_slice(&ELF_MAGIC);
    bytes[4] = ELFCLASS64;
    bytes[5] = ELFDATA2LSB;
    bytes[6] = EV_CURRENT;
    write_u16(&mut bytes, 16, ET_EXEC);
    write_u16(&mut bytes, 18, EM_RISCV);
    write_u32(&mut bytes, 20, 1);
    write_u64(&mut bytes, 24, 0x10bcc);
    write_u64(&mut bytes, 32, 64);
    write_u16(&mut bytes, 52, 64);
    write_u16(&mut bytes, 54, 56);
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
        120,
        PT_LOAD,
        PF_R | PF_X,
        0,
        BASE_LOAD_VADDR,
        0xfa1,
        0xfa1,
        FIX_PAGE,
    );
    write_phdr(&mut bytes, 176, PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16);
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
    tx_test_support::init_host();
    let _ = zones::register_all();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for exec_script tests: {error:?}"),
    }
    tx_test_support::drain_to_quiescence();
    reset_pid_counter();
    reset_tid_counter();
    reset_init_process();
    tx_subsystems::process::exec_prep::clear_cloexec_plan_allocation_fault_for_test();
    SCRIPTS_TEST_PMAP_STATE
        .lock()
        .expect("scripts pmap lock")
        .fail_next_create_root = false;
    TestSetup { _lock: lock }
}

/// Register a fresh `ExecTestFs` rootfs and return (root_dentry, fs).
fn build_fs_root_with_mount(
    mount_id: MountId,
) -> (Cap<DEntry>, Arc<ExecTestFs>, Cap<MountIdentity>) {
    let root_id = FsObjectId::new(2);
    let fs = ExecTestFs::new(root_id);

    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        fs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
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
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    let mount = MountIdentity::new_cap_with_root_dentry(
        mount_id,
        None,
        root_dentry.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    (root_dentry, fs, mount)
}

fn build_fs_root() -> (Cap<DEntry>, Arc<ExecTestFs>) {
    let (root_dentry, fs, _mount) = build_fs_root_with_mount(MountId::new(1));
    (root_dentry, fs)
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<ScriptsTestPmap>().expect("fresh aspace")
}

fn fresh_aspace_for<P: PmapIf>() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<P>().expect("fresh aspace")
}

fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll, Waker};

    let waker = Waker::noop().clone();
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

#[test]
fn absent_vdso_mapping_does_not_publish_auxv_address() {
    assert_eq!(super::vdso_auxv(None), None);
}

#[test]
fn image_read_out_of_memory_maps_to_enomem_for_main_and_interpreter() {
    let error = crate::process::exec::image_reader::ImageReadError::OutOfMemory;

    assert_eq!(
        ExecError::from_image_read_error(error),
        ExecError::OutOfMemory
    );
    assert_eq!(
        ExecError::from_interpreter_image_read_error(error),
        ExecError::OutOfMemory
    );
    assert_eq!(
        ExecError::OutOfMemory.to_step_errno(),
        step_engine::Errno::ENOMEM
    );
}

#[test]
fn image_read_io_maps_to_eio_for_main_and_interpreter() {
    let error = crate::process::exec::image_reader::ImageReadError::Io;

    assert_eq!(ExecError::from_image_read_error(error), ExecError::IoError);
    assert_eq!(
        ExecError::from_interpreter_image_read_error(error),
        ExecError::IoError
    );
    assert_eq!(ExecError::IoError.to_step_errno(), step_engine::Errno::EIO);
}

#[test]
fn exec_step_preserves_wait_source_yield_instead_of_returning_ebusy() {
    let shape = step_engine::YieldShape::on_wait_source(0x51, 0x3);

    assert_eq!(
        super::exec_result_to_step_outcome(Some(Err(ExecError::Deferred(shape)))),
        StepOutcome::Yield {
            progress: step_engine::NoProgress,
            shape,
        }
    );
    assert_eq!(
        super::exec_result_to_step_outcome(Some(Err(ExecError::Retry))),
        StepOutcome::Continue {
            progress: step_engine::NoProgress,
        }
    );
}

#[test]
fn real_page_container_eagain_makes_exec_script_op_continue_but_eio_stays_eio() {
    for (errno, expected) in [
        (
            tx_subsystems::execution::Errno::EAGAIN,
            StepOutcome::Continue {
                progress: step_engine::NoProgress,
            },
        ),
        (
            tx_subsystems::execution::Errno::EIO,
            StepOutcome::Err(step_engine::Errno::EIO),
        ),
    ] {
        let _setup = setup();
        let (process, thread) = bootstrap_with_errno_backed_file(b"init", errno);
        let argv: [&[u8]; 0] = [];
        let envp: [&[u8]; 0] = [];
        let cred = Credential::root();
        let mut op = super::ExecScriptOp::<ScriptsTestPmap>::new(
            &process, &thread, b"/init", &argv, &envp, &cred,
        );
        let mut ctx = step_engine::ScriptCtx::new();

        assert_eq!(op.step(&mut ctx), expected);
    }
}

#[test]
fn bss_tail_page_read_preserves_eagain_and_eio() {
    for (errno, expected) in [
        (
            tx_subsystems::execution::Errno::EAGAIN,
            StepOutcome::Continue {
                progress: step_engine::NoProgress,
            },
        ),
        (
            tx_subsystems::execution::Errno::EIO,
            StepOutcome::Err(step_engine::Errno::EIO),
        ),
    ] {
        let _setup = setup();
        let (process, thread) =
            bootstrap_with_offset_errno_backed_file(b"init", bss_tail_elf_bytes(), FIX_PAGE, errno);
        let argv: [&[u8]; 0] = [];
        let envp: [&[u8]; 0] = [];
        let cred = Credential::root();
        let mut op = super::ExecScriptOp::<ScriptsTestPmap>::new(
            &process, &thread, b"/init", &argv, &envp, &cred,
        );
        let mut ctx = step_engine::ScriptCtx::new();

        assert_eq!(op.step(&mut ctx), expected);
    }
}

#[test]
fn exec_fallible_buffer_and_layout_oom_map_to_enomem() {
    assert_eq!(
        super::try_zeroed_exec_bytes(usize::MAX),
        Err(ExecError::OutOfMemory)
    );
    assert_eq!(
        ExecError::Layout(crate::process::exec::loader::ElfLayoutError::OutOfMemory)
            .to_step_errno(),
        step_engine::Errno::ENOMEM
    );
    let mut items = Vec::<u8>::new();
    assert_eq!(
        super::try_reserve_exec_items(&mut items, usize::MAX),
        Err(ExecError::OutOfMemory)
    );
}

#[test]
fn fallible_layout_plan_clone_preserves_all_fields() {
    let plan = crate::process::exec::loader::parse_image_plan(&dynamic_elf_bytes(Some(b"/ld.so")))
        .expect("dynamic plan");

    let cloned = super::try_clone_exec_plan(&plan).expect("fallible plan clone");

    assert_eq!(cloned, plan);
}

#[test]
fn exec_main_rejects_noexec_child_mount_but_allows_root_mount_image() {
    let _setup = setup();
    let (process, thread, _root, root_fs, child_fs, _namespace, _child_mount) =
        bootstrap_with_child_mount(MountFlags::NOEXEC);
    let image = minimal_elf_bytes();
    root_fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &image);
    child_fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &image);
    let cred = Credential::root();

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/prog",
            &[],
            &[],
            &cred,
        )),
        Ok(())
    );
    let aspace_before = process.aspace_cap().expect("root exec aspace");
    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/mnt/prog",
            &[],
            &[],
            &cred,
        )),
        Err(ExecError::PermissionDenied)
    );
    assert_eq!(
        process.aspace_cap().expect("aspace after denial").key(),
        aspace_before.key()
    );
}

#[test]
fn exec_interpreter_rejects_noexec_child_mount() {
    let _setup = setup();
    let (process, thread, _root, root_fs, child_fs, _namespace, _child_mount) =
        bootstrap_with_child_mount(MountFlags::NOEXEC);
    root_fs.add_regular_with_bytes(
        FsObjectId::new(2),
        b"main",
        &dynamic_elf_bytes(Some(b"/mnt/ld.so")),
    );
    child_fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &dynamic_elf_bytes(None));

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/main",
            &[],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::PermissionDenied)
    );
}

#[test]
fn exec_symlink_crossing_to_noexec_mount_rejects_final_mount() {
    let _setup = setup();
    let (process, thread, _root, root_fs, child_fs, _namespace, _child_mount) =
        bootstrap_with_child_mount(MountFlags::NOEXEC);
    root_fs.add_symlink(FsObjectId::new(2), b"jump", b"/mnt/prog");
    child_fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &minimal_elf_bytes());

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/jump",
            &[],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::PermissionDenied)
    );
}

#[test]
fn relative_exec_uses_mounted_cwd_origin_across_setns() {
    let _setup = setup();
    let (process, thread, root, root_fs, child_fs, namespace, child_mount) =
        bootstrap_with_child_mount(MountFlags::NOEXEC);
    let image = minimal_elf_bytes();
    root_fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &image);
    child_fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &image);
    match step_chdir_with_mount(
        &process,
        child_mount.root_dentry().clone(),
        child_mount.clone(),
    ) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted child cwd rejected"),
    }
    let replacement = MountNamespace::new_cap(namespace.root().clone()).expect("replacement ns");
    step_set_mount_namespace(&process, replacement).expect("setns replacement");
    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"prog",
            &[],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::PermissionDenied)
    );

    let root_mount = namespace.root().clone();
    match step_chdir_with_mount(&process, root, root_mount) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted root cwd rejected"),
    }
    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"prog",
            &[],
            &[],
            &Credential::root(),
        )),
        Ok(())
    );
}

#[test]
fn relative_exec_rejects_legacy_cwd_without_mount_origin() {
    let _setup = setup();
    let (root, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    fs.add_regular_with_bytes(FsObjectId::new(2), b"prog", &minimal_elf_bytes());
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir(&process, root) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("legacy cwd rejected"),
    }
    step_set_mount_namespace(
        &process,
        MountNamespace::new_cap(mount).expect("mount namespace"),
    )
    .expect("install namespace");

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"prog",
            &[],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::PathNotFound)
    );
}

/// Bootstrap an init process whose cwd is the registered fs root, and
/// register a regular file at `/<name>` containing `bytes`. Returns
/// the leader thread, the process Cap, and the fs handle (for further
/// fixture mutation if needed).
fn bootstrap_with_file(
    name: &[u8],
    bytes: &[u8],
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecTestFs>) {
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), name, bytes);

    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");

    // Bind cwd so the walker has a search root.
    match step_chdir_with_mount(&process, root_dentry, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    (process, thread, fs)
}

fn bootstrap_with_file_for<P: PmapIf>(
    name: &[u8],
    bytes: &[u8],
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecTestFs>) {
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), name, bytes);

    let process = bootstrap_init_process(fresh_aspace_for::<P>()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root_dentry, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    (process, thread, fs)
}

fn bootstrap_with_errno_backed_file(
    name: &[u8],
    errno: tx_subsystems::execution::Errno,
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let backing = Arc::new(ErrnoPageBacking(errno));
    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        backing as Arc<dyn FsPageBacking>,
        None,
        DevId::new(199),
        MountOptions::default(),
        "exec-error-backing",
        SourceLabel::Static("exec-error-backing"),
    )
    .expect("error backing mount payload");
    let pc = PageContainer::new_file_cap(
        MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(payload)),
        FsObjectId::new(900),
        64,
    )
    .expect("error-backed page container");
    fs.add_regular_with_container(FsObjectId::new(2), name, pc, 64);

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root_dentry, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");
    (process, thread)
}

fn bootstrap_with_offset_errno_backed_file(
    name: &[u8],
    bytes: Vec<u8>,
    fail_offset: u64,
    errno: tx_subsystems::execution::Errno,
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let file_size = bytes.len() as u64;
    let backing = Arc::new(OffsetErrnoPageBacking {
        bytes,
        fail_offset,
        errno,
    });
    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        backing as Arc<dyn FsPageBacking>,
        None,
        DevId::new(200),
        MountOptions::default(),
        "exec-offset-error-backing",
        SourceLabel::Static("exec-offset-error-backing"),
    )
    .expect("offset error backing mount payload");
    let pc = PageContainer::new_file_cap(
        MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(payload)),
        FsObjectId::new(901),
        file_size,
    )
    .expect("offset error-backed page container");
    fs.add_regular_with_container(FsObjectId::new(2), name, pc, file_size);

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root_dentry, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");
    (process, thread)
}

fn bootstrap_with_child_mount(
    child_flags: MountFlags,
) -> (
    Cap<ProcessIdentity>,
    Cap<ThreadIdentity>,
    Cap<DEntry>,
    Arc<ExecTestFs>,
    Arc<ExecTestFs>,
    Cap<MountNamespace>,
    Cap<MountIdentity>,
) {
    tx_subsystems::mount::reset_mount_table_for_test();
    let (root_dentry, root_fs, root_mount) = build_fs_root_with_mount(MountId::new(1));
    root_fs.add_directory(FsObjectId::new(2), b"mnt");
    let cred = Credential::root();
    let guard = guard();
    let mountpoint =
        match tx_subsystems::vfs::walker::step_walk(root_dentry.clone(), b"/mnt", &cred, &guard) {
            StepOutcome::Done(dentry) => dentry,
            other => panic!("resolve child mountpoint: {other:?}"),
        };
    drop(guard);

    let child_root_id = FsObjectId::new(2);
    let child_fs = ExecTestFs::new(child_root_id);
    let child_payload = MountPayload::new_cap(
        child_fs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        child_fs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(100),
        MountOptions::default(),
        "exec-child-fs",
        SourceLabel::Static("exec-child"),
    )
    .expect("child mount payload");
    let child_root = {
        let raw = RNode::new(
            child_root_id,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&child_payload);
        let reservation = reserve_for::<RNode>().expect("child root reservation");
        sign_for(reservation, raw)
    };
    let child_mount = MountIdentity::new_cap(
        MountId::new(2),
        Some(mountpoint.clone()),
        child_root,
        Some(root_mount.clone()),
        child_payload,
        child_flags,
    )
    .expect("child mount");
    let namespace = MountNamespace::new_cap(root_mount.clone()).expect("mount namespace");
    namespace.register_mount(&mountpoint, child_mount.clone());

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root_dentry.clone(), root_mount) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    step_set_mount_namespace(&process, namespace.clone()).expect("install mount namespace");

    (
        process,
        thread,
        root_dentry,
        root_fs,
        child_fs,
        namespace,
        child_mount,
    )
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn exec_script_loads_minimal_elf_seeds_saved_user_context() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, fs) = bootstrap_with_file(b"init", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::root();

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

    let exe = process
        .exe_file()
        .expect("exec must publish exe_file for /proc/<pid>/exe");
    assert_eq!(exe.name().as_bytes(), b"init");

    let auxv = read_initial_auxv(&process, &thread);
    assert_eq!(read_user_cstring(&process, auxv[&15]), b"riscv64");
    assert_eq!(read_user_cstring(&process, auxv[&31]), b"/init");
}

#[test]
fn exec_script_la64_stack_publishes_platform_and_original_execfn() {
    let _setup = setup();
    let mut bytes = minimal_elf_bytes();
    bytes[18..20].copy_from_slice(&EM_LOONGARCH.to_le_bytes());
    let (process, thread, _fs) = bootstrap_with_file_for::<ScriptsLa64TestPmap>(b"la-init", &bytes);

    assert_eq!(
        block_on(exec_script::<ScriptsLa64TestPmap>(
            &process,
            &thread,
            b"/la-init",
            &[b"/la-init"],
            &[],
            &Credential::root(),
        )),
        Ok(())
    );

    let context = thread
        .payload_cap()
        .and_then(|payload| payload.saved_user_context())
        .expect("LA64 saved user context");
    let auxv = read_initial_auxv_at_sp(&process, context.regs[3] as u64);
    assert_eq!(read_user_cstring(&process, auxv[&15]), b"loongarch64");
    assert_eq!(read_user_cstring(&process, auxv[&31]), b"/la-init");
}

#[test]
fn exec_script_absolute_path_uses_process_mount_namespace() {
    let _setup = setup();
    let (cwd_root, _cwd_fs, _cwd_mount) = build_fs_root_with_mount(MountId::new(10));
    let (_namespace_root, namespace_fs, namespace_mount) =
        build_fs_root_with_mount(MountId::new(11));
    let bytes = minimal_elf_bytes();
    let _ = namespace_fs.add_regular_with_bytes(FsObjectId::new(2), b"init", &bytes);

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir(&process, cwd_root) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(namespace_mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[b"/init"],
        &[],
        &Credential::root(),
    ));

    assert_eq!(result, Ok(()));
}

#[test]
fn dynamic_exec_absolute_paths_do_not_require_cwd() {
    let _setup = setup();
    let (_namespace_root, namespace_fs, namespace_mount) =
        build_fs_root_with_mount(MountId::new(12));
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let interpreter = dynamic_elf_bytes(None);
    let _ = namespace_fs.add_regular_with_bytes(FsObjectId::new(2), b"main", &main);
    let _ = namespace_fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &interpreter);

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    let namespace = MountNamespace::new_cap(namespace_mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Ok(()));
}

#[test]
fn dynamic_exec_interpreter_ignores_same_path_in_other_cwd_root() {
    let _setup = setup();
    let (cwd_root, cwd_fs, _cwd_mount) = build_fs_root_with_mount(MountId::new(13));
    let (_namespace_root, namespace_fs, namespace_mount) =
        build_fs_root_with_mount(MountId::new(14));
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let interpreter = dynamic_elf_bytes(None);
    let _ = namespace_fs.add_regular_with_bytes(FsObjectId::new(2), b"main", &main);
    let _ = namespace_fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &interpreter);
    let _ = cwd_fs.add_directory(FsObjectId::new(2), b"ld.so");

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir(&process, cwd_root) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(namespace_mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Ok(()));
}

#[test]
fn dynamic_exec_layout_pc_auxv_and_ranges_do_not_overlap() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let interpreter = dynamic_elf_bytes(None);
    let (process, thread, fs) = bootstrap_with_file(b"main", &main);
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &interpreter);

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Ok(()));

    let context = thread
        .payload_cap()
        .and_then(|payload| payload.saved_user_context())
        .expect("dynamic entry context");
    let auxv = read_initial_auxv(&process, &thread);
    let at_base = auxv[&7];
    let at_entry = auxv[&9];
    let at_phdr = auxv[&3];

    assert_eq!(context.pc as u64, at_base + ENTRY_OFFSET);
    assert_eq!(at_phdr, at_entry - ENTRY_OFFSET + 64);
    assert_ne!(at_base, at_entry - ENTRY_OFFSET);

    let aspace = process.aspace_cap().expect("dynamic aspace");
    assert!(aspace.lookup(UserVirtAddr(context.pc)).is_some());
    assert!(aspace.lookup(UserVirtAddr(at_entry as usize)).is_some());
    assert!(aspace.lookup(UserVirtAddr(context.regs[2])).is_some());

    let recipes = aspace.recipes_snapshot();
    for (index, left) in recipes.iter().enumerate() {
        assert!(left.range.end().as_usize() <= ScriptsTestPmap::USER_TOP.0);
        for right in &recipes[index + 1..] {
            assert!(
                !left.range.overlaps(right.range),
                "dynamic main/interpreter/stack/vDSO recipes overlap: {:?} {:?}",
                left.range,
                right.range
            );
        }
    }
}

#[test]
fn dynamic_exec_layout_reserves_actual_vdso_window_on_large_user_va_platform() {
    let plan = crate::process::exec::loader::parse_image_plan(&dynamic_elf_bytes(None))
        .expect("dynamic plan");
    let layout =
        super::select_combined_layout(plan, None, 1u64 << 46, || 0).expect("large user VA layout");
    let vdso_layout = tx_subsystems::vm::VdsoLayout::for_user_top(
        tx_subsystems::vm::UserVirtAddr::new(1usize << 46),
    )
    .expect("large user top has a vDSO layout");
    assert_eq!(
        layout.vdso_window.vaddr,
        vdso_layout.window().start().as_usize() as u64
    );
}

#[test]
fn dynamic_exec_layout_keeps_et_exec_interpreter_at_fixed_addresses() {
    let main = crate::process::exec::loader::parse_image_plan(&dynamic_elf_bytes(None))
        .expect("dynamic main plan");
    let interpreter = crate::process::exec::loader::parse_image_plan(&minimal_elf_bytes())
        .expect("fixed interpreter plan");
    let layout = super::select_combined_layout(main, Some(interpreter), 0x8000_0000, || 1)
        .expect("fixed interpreter layout");
    let interpreter = layout.interpreter.expect("interpreter");

    assert_eq!(interpreter.load_bias, 0);
    assert_eq!(interpreter.entry, BASE_LOAD_VADDR + ENTRY_OFFSET);
    assert_eq!(interpreter.load_segments[0].vaddr, BASE_LOAD_VADDR);
}

#[test]
fn dynamic_exec_layout_exhausts_when_fixed_main_and_interpreter_overlap() {
    let main = crate::process::exec::loader::parse_image_plan(&minimal_elf_bytes())
        .expect("fixed main plan");
    let interpreter = crate::process::exec::loader::parse_image_plan(&minimal_elf_bytes())
        .expect("fixed interpreter plan");

    assert!(matches!(
        super::select_combined_layout(main, Some(interpreter), 0x8000_0000, || 0),
        Err(crate::process::exec::loader::ElfLayoutError::Exhausted)
    ));
}

fn large_dynamic_layout_plan() -> crate::process::exec::loader::ExecImagePlan {
    let mut plan = crate::process::exec::loader::parse_image_plan(&dynamic_elf_bytes(None))
        .expect("dynamic plan");
    plan.load_segments[0].memsz = 32 * 1024 * 1024;
    plan
}

#[test]
fn dynamic_exec_layout_retries_after_recoverable_candidate_error() {
    let calls = core::cell::Cell::new(0usize);
    let layout =
        super::select_combined_layout(large_dynamic_layout_plan(), None, 64 * 1024 * 1024, || {
            let call = calls.get();
            calls.set(call + 1);
            if call == 0 {
                u64::MAX
            } else {
                0
            }
        })
        .expect("second layout candidate should fit");

    assert_eq!(
        calls.get(),
        4,
        "selector must advance to a second candidate"
    );
    assert!(layout.main.load_segments[0].vaddr < layout.stack_top);
}

#[test]
fn dynamic_exec_layout_reports_exhausted_after_all_candidates_fail() {
    assert!(matches!(
        super::select_combined_layout(large_dynamic_layout_plan(), None, 64 * 1024 * 1024, || {
            u64::MAX
        },),
        Err(crate::process::exec::loader::ElfLayoutError::Exhausted)
    ));
}

#[test]
fn dynamic_exec_layout_malformed_main_is_enoexec() {
    let _setup = setup();
    let mut malformed = vec![0u8; 64];
    malformed[..4].copy_from_slice(&ELF_MAGIC);
    let (process, thread, _fs) = bootstrap_with_file(b"main", &malformed);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::NotExecutable));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ENOEXEC
    );
}

#[test]
fn dynamic_exec_layout_malformed_interpreter_is_elibbad() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let mut malformed = vec![0u8; 64];
    malformed[..4].copy_from_slice(&ELF_MAGIC);
    let (process, thread, fs) = bootstrap_with_file(b"main", &main);
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &malformed);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::InterpreterMalformed));
    assert_eq!(result.unwrap_err().to_errno_i32(), -80);
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ELIBBAD
    );
}

#[test]
fn dynamic_exec_layout_non_page_backed_interpreter_is_elibbad() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let (process, thread, fs) = bootstrap_with_file(b"main", &main);
    let _ = fs.add_non_page_backed_regular(FsObjectId::new(2), b"ld.so", 0o755, 0, 0);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::InterpreterMalformed));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ELIBBAD
    );
}

#[test]
fn dynamic_exec_layout_directory_interpreter_is_eacces() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let (process, thread, fs) = bootstrap_with_file(b"main", &main);
    let _ = fs.add_directory(FsObjectId::new(2), b"ld.so");
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::PermissionDenied));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::EACCES
    );
}

#[test]
fn dynamic_exec_rejects_non_executable_interpreter_with_eacces() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let interpreter = dynamic_elf_bytes(None);
    let (process, thread, fs) = bootstrap_with_file_meta(b"main", &main, 0o555, 0, 0);
    let _ = fs.add_regular_with_bytes_meta(FsObjectId::new(2), b"ld.so", &interpreter, 0o644, 0, 0);
    set_non_root_cred(&process, 1001);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &walker_cred_for(1001, 1001),
    ));
    assert_eq!(result, Err(ExecError::PermissionDenied));
}

#[test]
fn dynamic_exec_accepts_execute_only_interpreter() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let interpreter = dynamic_elf_bytes(None);
    let (process, thread, fs) = bootstrap_with_file_meta(b"main", &main, 0o555, 0, 0);
    let _ = fs.add_regular_with_bytes_meta(FsObjectId::new(2), b"ld.so", &interpreter, 0o111, 0, 0);
    set_non_root_cred(&process, 1001);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &walker_cred_for(1001, 1001),
    ));
    assert_eq!(result, Ok(()));
}

#[test]
fn dynamic_exec_layout_nested_interpreter_is_elibbad() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/ld.so"));
    let nested = dynamic_elf_bytes(Some(b"/nested.so"));
    let (process, thread, fs) = bootstrap_with_file(b"main", &main);
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), b"ld.so", &nested);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::InterpreterNested));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ELIBBAD
    );
}

#[test]
fn dynamic_exec_layout_missing_interpreter_is_enoent() {
    let _setup = setup();
    let main = dynamic_elf_bytes(Some(b"/missing.so"));
    let (process, thread, _fs) = bootstrap_with_file(b"main", &main);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/main",
        &[b"/main"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::PathNotFound));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ENOENT
    );
}

#[test]
fn interpreter_lookup_paths_uses_only_the_declared_path() {
    assert_eq!(
        super::interpreter_lookup_paths(b"/lib/ld-linux-riscv64-lp64d.so.1"),
        vec![b"/lib/ld-linux-riscv64-lp64d.so.1".to_vec()]
    );
}

#[test]
fn exec_script_loads_static_riscv_toolchain_elf_shape() {
    let _setup = setup();
    let bytes = static_toolchain_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"probe", &bytes);
    let cred = Credential::root();

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/probe",
        &[],
        &[],
        &cred,
    ));

    assert_eq!(result, Ok(()));
    assert_eq!(
        thread
            .payload_cap()
            .expect("thread payload")
            .saved_user_context()
            .expect("exec context")
            .pc as u64,
        0x10bcc
    );
}

#[test]
fn exec_script_collapses_sibling_threads_before_aspace_swap() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let sibling = tx_subsystems::process::execution::step_clone_thread(
        &process,
        &tx_hal::UserTrapContext {
            regs: [0; 32],
            pc: 0x4000_0000,
            status: 0,
            fp: tx_hal::UserFpContext::empty(),
        },
        tx_subsystems::signal::SignalMask::EMPTY,
        0,
        0,
        0,
    )
    .expect("sibling thread");
    assert_eq!(process.live_thread_count(), 2);

    let cred = Credential::root();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    assert_eq!(
        process.live_thread_count(),
        1,
        "exec must leave only the initiating thread live"
    );
    assert!(process.thread_by_tid(thread.tid.0).is_some());
    assert!(process.thread_by_tid(sibling.tid.0).is_none());
    assert!(sibling.is_zombie());
}

#[test]
fn initial_user_context_uses_arch_specific_stack_register() {
    assert_eq!(super::initial_user_sp_reg_for_arch(Arch::Riscv64), 2);
    assert_eq!(super::initial_user_sp_reg_for_arch(Arch::LoongArch64), 3);

    let ctx = super::make_initial_user_trap_context(Arch::LoongArch64, 0x1000, 0x4000);
    assert_eq!(ctx.regs[3], 0x4000);
    assert_eq!(ctx.regs[2], 0);
}

#[test]
fn exec_script_resets_brk_base_from_image_plan() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let cred = Credential::root();
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
fn exec_script_invalid_elf_returns_enoexec_without_kernel_shell_fallback() {
    let _setup = setup();
    // 4 KiB of zeroes — fails ELF magic check immediately.
    let bytes = vec![0u8; 4096];
    let (process, thread, _fs) = bootstrap_with_file(b"bad", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::root();
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
fn exec_script_oscomp_lmbench_hello_wrapper_uses_bin_sh() {
    let _setup = setup();
    let (root, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let bin = fs.add_directory(FsObjectId::new(2), b"bin");
    fs.add_regular_with_bytes(bin, b"sh", &minimal_elf_bytes());
    let tmp = fs.add_directory(FsObjectId::new(2), b"tmp");
    fs.add_regular_with_bytes(
        tmp,
        b"hello",
        b"/code/lmbench_src/bin/build/lmbench_all hello \"$@\"\n",
    );

    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted cwd rejected"),
    }
    step_set_mount_namespace(
        &process,
        MountNamespace::new_cap(mount).expect("mount namespace"),
    )
    .expect("install namespace");

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/tmp/hello",
            &[b"/tmp/hello", b"argument"],
            &[],
            &Credential::root(),
        )),
        Ok(())
    );
    assert_eq!(&process.comm()[..2], b"sh");
}

#[test]
fn shebang_busybox_sh_normalization_consumes_applet_arg() {
    let header = b"#!/bin/busybox sh\n./lua $1\n";
    let (interp, opt_arg) = super::shebang_parse(header).expect("valid shebang");
    let original_argv: [&[u8]; 2] = [b"./test.sh", b"date.lua"];

    let (interp_path, argv) =
        super::shebang_exec_argv(interp, opt_arg, b"./test.sh", &original_argv)
            .expect("shebang argv");

    assert_eq!(interp_path, b"/bin/sh");
    let argv_refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    assert_eq!(
        argv_refs,
        vec![b"/bin/sh".as_slice(), b"./test.sh", b"date.lua"]
    );
}

#[test]
fn shebang_optional_argument_preserves_trimmed_remaining_text() {
    let header = b"#!/usr/bin/env   -S python -O  \n";
    let (interp, opt_arg) = super::shebang_parse(header).expect("valid shebang");

    assert_eq!(interp, b"/usr/bin/env");
    assert_eq!(opt_arg, Some(b"-S python -O".as_slice()));
}

#[test]
fn shebang_without_newline_rejects_potentially_truncated_interpreter() {
    let mut header = vec![b'a'; 256];
    header[..2].copy_from_slice(b"#!");

    assert!(super::shebang_parse(&header).is_none());
}

#[test]
fn shebang_without_newline_accepts_truncated_optional_text() {
    let mut header = vec![b'x'; 256];
    let prefix = b"#!/usr/bin/env -S python -O ";
    header[..prefix.len()].copy_from_slice(prefix);

    let (interp, opt_arg) = super::shebang_parse(&header).expect("complete interpreter path");
    assert_eq!(interp, b"/usr/bin/env");
    assert_eq!(opt_arg, Some(&header[b"#!/usr/bin/env ".len()..255]));
}

#[test]
fn shebang_newline_at_binprm_buffer_boundary_is_valid() {
    let mut header = vec![b' '; 256];
    let prefix = b"#!/bin/sh";
    header[..prefix.len()].copy_from_slice(prefix);
    header[255] = b'\n';

    assert_eq!(
        super::shebang_parse(&header),
        Some((b"/bin/sh".as_slice(), None))
    );
}

#[test]
fn exec_script_rejects_embedded_nul_in_argv_and_envp() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/init",
            &[b"init\0tail"],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::InvalidArgument)
    );
    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/init",
            &[b"init"],
            &[b"KEY=value\0tail"],
            &Credential::root(),
        )),
        Err(ExecError::InvalidArgument)
    );
}

#[test]
fn exec_script_resolves_multiple_shebang_layers_iteratively() {
    let _setup = setup();
    let (root, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    fs.add_regular_with_bytes(FsObjectId::new(2), b"s1", b"#!/s2\n");
    fs.add_regular_with_bytes(FsObjectId::new(2), b"s2", b"#!/bin/sh\n");
    let bin = fs.add_directory(FsObjectId::new(2), b"bin");
    fs.add_regular_with_bytes(bin, b"sh", &minimal_elf_bytes());
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted cwd rejected"),
    }
    step_set_mount_namespace(
        &process,
        MountNamespace::new_cap(mount).expect("mount namespace"),
    )
    .expect("install namespace");

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/s1",
            &[b"/s1"],
            &[],
            &Credential::root(),
        )),
        Ok(())
    );

    let auxv = read_initial_auxv(&process, &thread);
    assert_eq!(read_user_cstring(&process, auxv[&15]), b"riscv64");
    assert_eq!(read_user_cstring(&process, auxv[&31]), b"/s1");
}

#[test]
fn exec_script_fifth_shebang_redirect_returns_eloop() {
    let _setup = setup();
    let (root, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    for index in 1..=5 {
        let name = alloc::format!("s{index}");
        let next = alloc::format!("#!/s{}\n", index + 1);
        fs.add_regular_with_bytes(FsObjectId::new(2), name.as_bytes(), next.as_bytes());
    }
    fs.add_regular_with_bytes(FsObjectId::new(2), b"s6", &minimal_elf_bytes());
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted cwd rejected"),
    }
    step_set_mount_namespace(
        &process,
        MountNamespace::new_cap(mount).expect("mount namespace"),
    )
    .expect("install namespace");

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/s1",
        &[b"/s1"],
        &[],
        &Credential::root(),
    ));
    assert_eq!(result, Err(ExecError::SymlinkLoop));
    assert_eq!(
        result.unwrap_err().to_step_errno(),
        step_engine::Errno::ELOOP
    );
}

#[test]
fn exec_script_four_shebang_redirects_succeed() {
    let _setup = setup();
    let (root, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    for index in 1..=4 {
        let name = alloc::format!("s{index}");
        let next = alloc::format!("#!/s{}\n", index + 1);
        fs.add_regular_with_bytes(FsObjectId::new(2), name.as_bytes(), next.as_bytes());
    }
    fs.add_regular_with_bytes(FsObjectId::new(2), b"s5", &minimal_elf_bytes());
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted cwd rejected"),
    }
    step_set_mount_namespace(
        &process,
        MountNamespace::new_cap(mount).expect("mount namespace"),
    )
    .expect("install namespace");

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/s1",
            &[b"/s1"],
            &[],
            &Credential::root(),
        )),
        Ok(())
    );
}

#[test]
fn exec_script_path_not_found_returns_path_not_found() {
    let _setup = setup();
    // Register a file but exec a different path so the walker
    // returns ENOENT before any read or parse.
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let aspace_before = process.aspace_cap().expect("alive aspace");
    let cred = Credential::root();
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
    let _ = step_sigaction(&process, Signum::SIGTERM, SigDisposition::handler(0xdead));
    assert_eq!(
        process
            .sig_disposition(Signum::SIGTERM)
            .expect("SIGTERM disposition pre-exec"),
        SigDisposition::handler(0xdead),
    );

    let cred = Credential::root();
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

    use crate::adapter::vfs_exec::{OpenFile, OpenFileFlags};
    // Build OpenFiles directly over the test fs's PageContainers via
    // `materialise_rnode` — tx-subsystems::vfs::OpenFile::new_cap is
    // public.
    let open_for = |name: &[u8]| -> Cap<OpenFile> {
        // Resolve through the walker; it will go through
        // materialise_rnode and produce a PageBacked RNode. Use the
        // root walker cred so the test isn't gated on tmpfs's mode
        // bits — the slice's DAC test coverage lives elsewhere.
        use step_engine::StepOutcome as V3;
        let cred = Credential::root();
        let cwd = process.cwd().expect("cwd bound");
        let guard = guard();
        let outcome = tx_subsystems::vfs::walker::step_open(
            cwd,
            name,
            OpenFileFlags {
                read: true,
                write: false,
                append: false,
                cloexec: false,
                nonblocking: false,
                packet: false,
            },
            0,
            &cred,
            &guard,
        );
        drop(guard);
        match outcome {
            V3::Done(file) => file,
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

    let cred = Credential::root();
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

#[test]
fn cloexec_plan_oom_is_pre_ponr_and_preserves_old_aspace_and_fds() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
    let old_aspace_key = process.aspace_cap().expect("old aspace").key();
    let cwd = process.cwd().expect("cwd bound");
    let guard = guard();
    let marked = match tx_subsystems::vfs::walker::step_open(
        cwd,
        b"/init",
        crate::adapter::vfs_exec::OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
        0,
        &Credential::root(),
        &guard,
    ) {
        StepOutcome::Done(file) => file,
        other => panic!("open CLOEXEC fixture: {other:?}"),
    };
    drop(guard);
    let marked_key = marked.key();
    process.set_fd(3, Some(marked));
    process.set_fd_cloexec(3, true);
    tx_subsystems::process::exec_prep::fail_next_cloexec_plan_allocation_for_test();

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/init",
            &[],
            &[],
            &Credential::root(),
        )),
        Err(ExecError::OutOfMemory),
    );
    assert_eq!(
        process.aspace_cap().expect("old aspace retained").key(),
        old_aspace_key
    );
    assert_eq!(process.fd(3).expect("marked fd retained").key(), marked_key);
    assert!(process.fd_cloexec(3));
}

// ---------------------------------------------------------------------------
// Wave 4 Part 5: pre-Phase-6 X-bit auth + Phase 3.5 setuid recompute.
// ---------------------------------------------------------------------------

/// Bootstrap an init process with a fixture file at `/<name>` whose
/// inode mode bits (without S_IFMT) and uid/gid are configurable. Used
/// by the Wave 4 Part 5 tests to drive the X-bit and setuid paths.
fn bootstrap_with_file_meta(
    name: &[u8],
    bytes: &[u8],
    mode_bits: u16,
    uid: u32,
    gid: u32,
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecTestFs>) {
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let _ = fs.add_regular_with_bytes_meta(FsObjectId::new(2), name, bytes, mode_bits, uid, gid);

    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");

    match step_chdir_with_mount(&process, root_dentry, mount.clone()) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("init bootstrap somehow zombified"),
    }
    let namespace = MountNamespace::new_cap(mount).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install process mount namespace");

    (process, thread, fs)
}

/// Drop the bootstrap process from "root + FULL caps" to a chosen
/// non-root cred. Step 1: clear effective/permitted caps via the
/// cross-crate test seam (otherwise `is_privileged_for(CAP_SETUID)`
/// short-circuits even after the uid swap). Step 2: drive the
/// privileged `step_setresuid` while still root to install the new
/// real/effective/saved uid set. Mirrors the cred/tests.rs
/// `drop_to` helper but routed through public mutators since
/// `payload` is `pub(crate)` to tx-scripts.
fn set_non_root_cred(process: &Cap<ProcessIdentity>, uid: u32) {
    use tx_subsystems::cred::{step_setresuid, CredChange, Uid as CredUid};
    let outcome = step_setresuid(
        process,
        Some(CredUid(uid)),
        Some(CredUid(uid)),
        Some(CredUid(uid)),
    );
    assert!(matches!(outcome, CredChange::Replaced { .. }));
    tx_subsystems::cross_crate_test_support::clear_caps_for_test(process);
}

/// Build a `Credential` for the walker step matching the process's
/// post-drop cred. Used so the walker DAC checks (Wave 3) and the
/// Wave 4 X-bit check both see the same caller identity.
fn walker_cred_for(uid: u32, gid: u32) -> Credential {
    Credential {
        uid,
        gid,
        effective_caps: tx_subsystems::cred::CapabilitySet::EMPTY,
    }
}

#[test]
fn exec_script_eacces_for_non_executable_binary() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // mode 0o600 — no X bit anywhere. Owned by uid 0; caller is uid 1001
    // with no capabilities. Falls through to "other" triplet → no X → EACCES.
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o600, 0, 0);
    set_non_root_cred(&process, 1001);

    let cred = walker_cred_for(1001, 1001);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Err(ExecError::PermissionDenied));
}

#[test]
fn exec_script_accepts_execute_only_main_candidate() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o111, 0, 0);
    set_non_root_cred(&process, 1001);

    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &walker_cred_for(1001, 1001),
    ));
    assert_eq!(result, Ok(()));
}

#[test]
fn exec_script_eacces_for_other_user_when_no_other_x_bit() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // mode 0o710: rwx-/--x/--- — owner+group can X, other cannot.
    // File owned by uid 0; caller is uid 1001 (non-root, no caps),
    // which falls through to the "other" triplet → no X → EACCES.
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o710, 0, 0);
    set_non_root_cred(&process, 1001);

    let cred = walker_cred_for(1001, 1001);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Err(ExecError::PermissionDenied));
}

#[test]
fn exec_script_dac_override_bypasses_with_x_bit_set() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // mode 0o711: any X bit is set, so CAP_DAC_OVERRIDE bypasses the
    // ownership/triplet check. File owned by uid 0; caller is root
    // (effective_caps = FULL).
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o711, 0, 0);

    let cred = Credential::root();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));
}

#[test]
fn exec_script_dac_override_still_requires_some_x_bit() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // mode 0o600 has no execute bits. Linux still rejects exec under
    // CAP_DAC_OVERRIDE unless at least one execute bit is present.
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o600, 0, 0);

    let cred = Credential::root();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Err(ExecError::PermissionDenied));
}

#[test]
fn executable_permission_uses_live_inode_meta_after_cached_rnode_chmod() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let root_id = FsObjectId::new(2);
    let (root_dentry, fs, mount) = build_fs_root_with_mount(MountId::new(1));
    let file_id = fs.add_regular_with_bytes_meta(root_id, b"minibuild", &bytes, 0o644, 0, 0);
    let namespace = MountNamespace::new_cap(mount.clone()).expect("mount namespace");

    // Keep the pre-chmod dentry/RNode alive, reproducing the VFS cache shape
    // seen when Cargo/linker creates, stats and then chmods its output.
    let guard = guard();
    let stale_dentry = match tx_subsystems::vfs::step_walk_in_mount_namespace_with_origin_mount(
        root_dentry.clone(),
        &mount,
        b"/minibuild",
        &Credential::root(),
        &namespace,
        &guard,
    ) {
        StepOutcome::Done(resolved) => resolved.dentry,
        other => panic!("materialise pre-chmod dentry: {other:?}"),
    };
    drop(guard);
    assert_eq!(stale_dentry.rnode().meta().mode & 0o7777, 0o644);

    // The filesystem has committed chmod, while the cached RNode correctly
    // remains a snapshot.  Exec must consult live inode metadata and accept
    // the file without weakening Linux's root-requires-some-X rule.
    fs.set_regular_mode_bits(file_id, 0o755);
    let candidate = super::open_executable_candidate(
        root_dentry,
        &mount,
        b"/minibuild",
        &Credential::root(),
        &namespace,
        super::ExecutableCandidateRole::Main,
    );
    assert!(candidate.is_ok(), "live chmod mode must authorize exec");
    assert_eq!(stale_dentry.rnode().meta().mode & 0o7777, 0o644);
}

#[test]
fn exec_script_setuid_binary_changes_euid() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // S_ISUID | 0o755, owned by uid 1000. Caller is uid 1001 (non-
    // root). After exec the effective uid should be 1000 and the
    // saved-set uid should also be 1000.
    let mode_bits = 0o4755; // S_ISUID | rwxr-xr-x
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, mode_bits, 1000, 0);
    set_non_root_cred(&process, 1001);
    let before_cred_cap = process.cred_cap().expect("pre-exec cred cap");

    let cred = walker_cred_for(1001, 1001);
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    let post = process.cred().expect("alive process");
    assert_eq!(post.uid.raw(), 1001, "real uid preserved by exec");
    assert_eq!(post.euid.raw(), 1000, "S_ISUID applied");
    assert_eq!(post.suid.raw(), 1000, "saved-set tracks new euid");
    assert_ne!(
        process.cred_cap().expect("post-exec cred cap").key(),
        before_cred_cap.key(),
        "successful exec publishes the prepared cred"
    );
    assert_eq!(read_initial_auxv(&process, &thread)[&23], 1);
}

#[test]
fn exec_setid_prepare_rolls_back_when_aspace_build_fails() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o6755, 1000, 2000);
    set_non_root_cred(&process, 1001);
    tx_subsystems::cross_crate_test_support::set_cred_ids_for_test(
        &process, 1001, 1001, 1001, 1001, 1001, 1001,
    );
    let before = process.cred().expect("pre-exec cred");

    fail_next_pmap_root_creation();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &walker_cred_for(1001, 1001),
    ));

    assert_eq!(result, Err(ExecError::OutOfMemory));
    assert_eq!(
        process.cred().expect("failed exec keeps process alive"),
        before
    );
}

#[test]
fn exec_setuid_cred_allocation_oom_is_pre_ponr_and_returns_enomem() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o4755, 1000, 2000);
    set_non_root_cred(&process, 1001);
    let before_cred_cap = process.cred_cap().expect("pre-exec cred cap");
    let before_cred = *before_cred_cap;
    let before_aspace = process.aspace_cap().expect("pre-exec aspace");
    let before_context = thread
        .payload_cap()
        .and_then(|payload| payload.saved_user_context());

    tx_subsystems::cred::fail_next_cred_sign_for_test();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &walker_cred_for(1001, 1001),
    ));

    assert_eq!(result, Err(ExecError::OutOfMemory));
    assert_eq!(
        process.cred_cap().expect("post-error cred cap").key(),
        before_cred_cap.key()
    );
    assert_eq!(process.cred().expect("post-error cred"), before_cred);
    assert_eq!(
        process.aspace_cap().expect("post-error aspace").key(),
        before_aspace.key()
    );
    assert_eq!(
        thread
            .payload_cap()
            .and_then(|payload| payload.saved_user_context()),
        before_context
    );
}

#[test]
fn exec_setid_uses_final_alias_mount_nosuid_flag() {
    let _setup = setup();
    tx_subsystems::mount::reset_mount_table_for_test();
    let (root, fs, root_mount) = build_fs_root_with_mount(MountId::new(1));
    fs.add_directory(FsObjectId::new(2), b"alias");
    fs.add_regular_with_bytes_meta(
        FsObjectId::new(2),
        b"setid",
        &minimal_elf_bytes(),
        0o4755,
        1000,
        0,
    );
    let guard = guard();
    let mountpoint = match tx_subsystems::vfs::walker::step_walk(
        root.clone(),
        b"/alias",
        &Credential::root(),
        &guard,
    ) {
        StepOutcome::Done(dentry) => dentry,
        other => panic!("resolve alias mountpoint: {other:?}"),
    };
    drop(guard);
    let payload = root_mount
        .payload_cap()
        .expect("root mount payload")
        .into_cap();
    let alias_mount = MountIdentity::new_cap_with_root_dentry(
        MountId::new(2),
        Some(mountpoint.clone()),
        root.clone(),
        Some(root_mount.clone()),
        payload,
        MountFlags::NOSUID,
    )
    .expect("nosuid alias mount");
    let namespace = MountNamespace::new_cap(root_mount.clone()).expect("namespace");
    namespace.register_mount(&mountpoint, alias_mount);
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir_with_mount(&process, root, root_mount) {
        ChdirOutcome::Replaced { .. } => {}
        ChdirOutcome::ZombieIgnored => panic!("mounted cwd rejected"),
    }
    step_set_mount_namespace(&process, namespace).expect("install namespace");
    set_non_root_cred(&process, 1001);
    let cred = walker_cred_for(1001, 1001);

    tx_subsystems::cred::fail_next_cred_sign_for_test();
    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/alias/setid",
            &[],
            &[],
            &cred,
        )),
        Ok(())
    );
    assert_eq!(process.cred().expect("post-nosuid cred").euid.raw(), 1001);
    assert_eq!(read_initial_auxv(&process, &thread)[&23], 0);

    assert!(matches!(
        tx_subsystems::cred::prepare_setid_for_exec(
            &process,
            tx_subsystems::cred::Uid(1000),
            tx_subsystems::cred::Gid(0),
            0o4755,
        ),
        Err(tx_subsystems::execution::Errno::ENOMEM)
    ));
    assert_eq!(
        process.cred().expect("post-prepare-oom cred").euid.raw(),
        1001
    );

    assert_eq!(
        block_on(exec_script::<ScriptsTestPmap>(
            &process,
            &thread,
            b"/setid",
            &[],
            &[],
            &cred,
        )),
        Ok(())
    );
    assert_eq!(process.cred().expect("post-normal cred").euid.raw(), 1000);
    assert_eq!(read_initial_auxv(&process, &thread)[&23], 1);
}

#[test]
fn exec_script_no_setuid_at_secure_zero() {
    let _setup = setup();
    let bytes = minimal_elf_bytes();
    // mode 0o755, owned by uid 0. Caller is root (default). No setuid
    // bit → cred unchanged → at_secure should be 0 in the auxv.
    let (process, thread, _fs) = bootstrap_with_file_meta(b"init", &bytes, 0o755, 0, 0);

    let pre = process.cred().expect("alive process");
    let cred = Credential::root();
    let result = block_on(exec_script::<ScriptsTestPmap>(
        &process,
        &thread,
        b"/init",
        &[],
        &[],
        &cred,
    ));
    assert_eq!(result, Ok(()));

    let post = process.cred().expect("alive process");
    assert_eq!(post.uid, pre.uid);
    assert_eq!(post.euid, pre.euid);
    assert_eq!(post.suid, pre.suid);
}
