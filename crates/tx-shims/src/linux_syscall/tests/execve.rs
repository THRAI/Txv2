// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use alloc::sync::Arc;
use alloc::vec;
use std::collections::BTreeMap;

use crate::adapter::step_engine::{
    self as step_engine, Cap, Errno as V3Errno, NoProgress, SpinMutex, StepOutcome, guard,
    page_allocator, reserve_for, sign_for,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::{
    AnonSwapPolicy, Frame, MaterializeAccess, PageContainer, PageContainerKind, PageIndex,
};
use tx_subsystems::process::step_chdir;
use tx_subsystems::vfs::structure::{
    Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking, S_IFDIR, S_IFREG,
};
use tx_subsystems::vm::USER_PAGE_SIZE;

// Minimal in-test FsOps fixture mirroring tx-scripts'
// `ExecTestFs`. We re-implement here rather than depending on
// tx-scripts' test-only types so the surface stays self-contained.

struct ExecveTestFs {
    inner: SpinMutex<ExecveTestFsInner>,
}

struct ExecveTestFsInner {
    children: BTreeMap<FsObjectId, BTreeMap<Vec<u8>, FsObjectId>>,
    inodes: BTreeMap<FsObjectId, ExecveTestInode>,
    next_id: u64,
}

enum ExecveTestInode {
    Directory,
    Regular {
        container: Cap<PageContainer>,
        size: u64,
    },
}

impl ExecveTestFs {
    fn new(root_id: FsObjectId) -> Arc<Self> {
        let mut inner = ExecveTestFsInner {
            children: BTreeMap::new(),
            inodes: BTreeMap::new(),
            next_id: root_id.as_u64() + 1,
        };
        inner.children.insert(root_id, BTreeMap::new());
        inner.inodes.insert(root_id, ExecveTestInode::Directory);
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

    fn add_regular_with_bytes(&self, parent: FsObjectId, name: &[u8], bytes: &[u8]) -> FsObjectId {
        let pages = (bytes.len() as u64).div_ceil(USER_PAGE_SIZE as u64);
        let pages = core::cmp::max(pages, 1);
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            pages,
        )
        .expect("page container reservation");
        for (idx, chunk) in bytes.chunks(USER_PAGE_SIZE).enumerate() {
            let materialised = pc
                .materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write)
                .expect("materialize anon page for fixture");
            let frame_base =
                page_allocator::frame_kernel_addr(materialised.ppn).expect("direct-map view");
            // SAFETY: freshly materialised anon page; the pin is
            // held via `materialised.map_pin` until the iteration
            // ends.
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
            }
        }
        let size = bytes.len() as u64;
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
            ExecveTestInode::Regular {
                container: pc,
                size,
            },
        );
        id
    }
}

// ----- Minimal RV64 ET_EXEC ELF fixture (mirrors
//       tx-scripts' `minimal_elf_bytes`). -----

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
        write_u64(b, at + 24, p_vaddr);
        write_u64(b, at + 32, p_filesz);
        write_u64(b, at + 40, p_memsz);
        write_u64(b, at + 48, p_align);
    }

    let phoff: u64 = 64;
    let n_phdrs: u16 = 2;
    let phent: u16 = 56;
    let total_phdrs = (n_phdrs as u64) * (phent as u64);
    let file_size: usize = (phoff + total_phdrs) as usize;
    let mut bytes = vec![0u8; file_size];

    bytes[0..4].copy_from_slice(&ELF_MAGIC);
    bytes[4] = ELFCLASS64;
    bytes[5] = ELFDATA2LSB;
    bytes[6] = EV_CURRENT;
    write_u16(&mut bytes, 16, ET_EXEC);
    write_u16(&mut bytes, 18, EM_RISCV);
    write_u32(&mut bytes, 20, 1);
    write_u64(&mut bytes, 24, BASE_LOAD_VADDR + ENTRY_OFFSET);
    write_u64(&mut bytes, 32, phoff);
    write_u64(&mut bytes, 40, 0);
    write_u32(&mut bytes, 48, 0);
    write_u16(&mut bytes, 52, 64);
    write_u16(&mut bytes, 54, phent);
    write_u16(&mut bytes, 56, n_phdrs);
    write_u16(&mut bytes, 58, 0);
    write_u16(&mut bytes, 60, 0);
    write_u16(&mut bytes, 62, 0);

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

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for execve tests: {error:?}"),
    }
}

fn build_fs_root() -> (Cap<DEntry>, Arc<ExecveTestFs>) {
    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);

    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        fs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(99),
        MountOptions::default(),
        "execve-test-fs",
        SourceLabel::Static("execve-test"),
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

fn bootstrap_with_file(
    name: &[u8],
    bytes: &[u8],
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>, Arc<ExecveTestFs>) {
    let (root_dentry, fs) = build_fs_root();
    let _ = fs.add_regular_with_bytes(FsObjectId::new(2), name, bytes);

    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");

    match step_chdir(&process, root_dentry) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
            panic!("init bootstrap somehow zombified")
        }
    }

    (process, thread, fs)
}

fn execve_setup() -> TestSetup {
    let setup = setup();
    ensure_zero_frame_claimed();
    setup
}

/// `execve("/nope", NULL, NULL)` against an empty fs returns
/// `-ENOENT` per the standard `ExecError::PathNotFound` mapping.
/// Verifies the syscall arm walks the path through the VFS walker
/// and surfaces the expected errno magnitude (positive 2 = ENOENT).
#[test]
fn dispatch_execve_path_not_found_returns_neg_enoent() {
    let _setup = execve_setup();

    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
    let ctx = make_ctx(process, thread);

    // path = "/nope" — present in fs but registered as `init`.
    let path: &[u8] = b"/nope\0";
    let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(2));
}

/// `execve` of a non-ELF file returns `-ENOEXEC` (positive 8) via
/// Non-ELF / non-shebang file → kernel-side fallback to `/bin/sh`.
/// Pre-2026-05-18 returned `-ENOEXEC (8)`; the new behaviour matches
/// every userspace shell's ENOEXEC fallback. The test fixture has no
/// `/bin/sh`, so the second exec attempt fails the walker with
/// `-ENOENT (2)`. The shape of the failure proves the fallback fires
/// (the libctest 0/220 unblock relies on this — see STATUS.md
/// 2026-05-18).
#[test]
fn dispatch_execve_non_elf_non_shebang_falls_back_to_bin_sh() {
    let _setup = execve_setup();

    let bytes = vec![0u8; 4096]; // 4 KiB of zeroes — fails ELF magic.
    let (process, thread, _fs) = bootstrap_with_file(b"bad", &bytes);
    let ctx = make_ctx(process, thread);

    let path: &[u8] = b"/bad\0";
    let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(
        result,
        SyscallResult::Error(2),
        "kernel-side ENOEXEC fallback should re-exec via /bin/sh; \
         the fixture has no /bin/sh so the second walker returns \
         -ENOENT (2) — not -ENOEXEC (8), which would mean the \
         fallback never fired."
    );
}

/// A path with no NUL terminator within `EXECVE_PATH_MAX = 4096`
/// returns `-ENAMETOOLONG` (positive 36) — the bounded user-buffer
/// copy short-circuits before any walker call.
#[test]
fn dispatch_execve_too_long_path_returns_neg_enametoolong() {
    let _setup = execve_setup();

    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
    let ctx = make_ctx(process, thread);

    // Allocate a kernel-side buffer of 4097 'A' bytes, no NUL.
    // `read_user_cstr` walks 4096 bytes and gives up.
    let path: Vec<u8> = vec![b'A'; super::super::EXECVE_PATH_MAX + 1];
    let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(36));
    // Keep the buffer alive until after the call.
    drop(path);
}

/// argv whose total byte budget exceeds `EXECVE_ARG_MAX_INLINE =
/// 8192` returns `-E2BIG` (positive 7) — the per-string read
/// helper accumulates against a shared budget.
#[test]
fn dispatch_execve_argv_overflow_returns_neg_e2big() {
    let _setup = execve_setup();

    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);
    let ctx = make_ctx(process, thread);

    // Build two argv strings whose combined length (including
    // implicit NUL accounting) exceeds 8 KiB. Each is 5 KiB +
    // trailing NUL.
    let big_arg: Vec<u8> = {
        let mut v = vec![b'X'; 5 * 1024];
        v.push(0);
        v
    };
    // argv array: [&big_arg, &big_arg, NULL]
    let argv_array: [u64; 3] = [big_arg.as_ptr() as u64, big_arg.as_ptr() as u64, 0];

    let path: &[u8] = b"/init\0";
    let req = SyscallRequest::new(
        NR_EXECVE,
        [path.as_ptr() as u64, argv_array.as_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(7));
    drop(big_arg);
}

/// Successful execve of a minimal ELF returns
/// `SyscallResult::ExecCommitted` AND swaps the process's
/// `aspace_cap` AND seeds the thread's `saved_user_context` with
/// the new image's `pc` / `sp`.
///
/// Asserts the contract Phase 6 introduces: the dispatcher signals
/// "do not write a syscall return; the new image's `_start` runs
/// next".
#[test]
fn dispatch_execve_success_returns_exec_committed() {
    let _setup = execve_setup();

    let bytes = minimal_elf_bytes();
    let (process, thread, _fs) = bootstrap_with_file(b"init", &bytes);

    let aspace_before_key = process.aspace_cap().expect("alive aspace").key();

    let ctx = make_ctx(process.clone(), thread.clone());

    let path: &[u8] = b"/init\0";
    let req = SyscallRequest::new(NR_EXECVE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(
        result,
        SyscallResult::ExecCommitted,
        "successful execve must surface ExecCommitted to the thread future"
    );

    // Phase 6 swap observable.
    let aspace_after = process.aspace_cap().expect("alive aspace post-exec");
    assert_ne!(
        aspace_before_key,
        aspace_after.key(),
        "aspace must have been atomically replaced"
    );

    // Saved user context populated.
    let payload = thread.payload_cap().expect("alive thread payload");
    let user_ctx = payload
        .saved_user_context()
        .expect("Phase 6 must seed saved_user_context");
    assert_eq!(
        user_ctx.pc as u64,
        BASE_LOAD_VADDR + ENTRY_OFFSET,
        "saved_user_context.pc must match the ELF entry"
    );
    // RV64 SP lives at register x2.
    assert_ne!(user_ctx.regs[2], 0, "initial sp must be populated");
    assert_eq!(
        user_ctx.regs[2] & 0xF,
        0,
        "initial sp must be 16-byte aligned"
    );
}

// === `FsOps` + `FsPageBacking` impls on `ExecveTestFs`. ===

impl tx_subsystems::vfs::FsOps for ExecveTestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return StepOutcome::err(Errno::ENOTDIR.into());
        };
        match map.get(name) {
            Some(id) => StepOutcome::done(*id),
            None => StepOutcome::err(Errno::ENOENT.into()),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let meta = match inode {
            ExecveTestInode::Directory => InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            ExecveTestInode::Regular { size, .. } => {
                let mut meta = InodeMeta::new(InodeKind::Regular, S_IFREG | 0o755);
                meta.size = *size;
                meta
            }
        };
        StepOutcome::done(meta)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        StepOutcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn read_link(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Box<[u8]>, NoProgress> {
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        match inode {
            ExecveTestInode::Regular { container, .. } => {
                match RNode::new_cap_in_mount(
                    fs_object_id,
                    meta,
                    RNodeBacking::PageBacked {
                        pc: container.clone(),
                    },
                    mount,
                ) {
                    Ok(rnode) => StepOutcome::done(rnode),
                    Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
                }
            }
            ExecveTestInode::Directory => StepOutcome::err(Errno::EISDIR.into()),
        }
    }
}

impl tx_subsystems::page_backed::FsPageBacking for ExecveTestFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        let inner = self.inner.lock();
        let container = match inner.inodes.get(&fs_object_id) {
            Some(ExecveTestInode::Regular { container, .. }) => container.clone(),
            Some(ExecveTestInode::Directory) => {
                return StepOutcome::err(Errno::EISDIR.into());
            }
            None => {
                return StepOutcome::err(Errno::ENOENT.into());
            }
        };
        drop(inner);

        let page_size = USER_PAGE_SIZE as u64;
        if !offset.is_multiple_of(page_size) {
            return StepOutcome::err(Errno::EINVAL.into());
        }
        let page_index = PageIndex::new(offset / page_size);
        use StepOutcome as V3;
        match container.materialize_page(page_index, MaterializeAccess::Read, guard) {
            V3::Done(materialised) => V3::done(Frame::new(materialised.ppn)),
            V3::Continue { .. } => V3::err(V3Errno::EAGAIN),
            V3::Yield { .. } => V3::err(V3Errno::EAGAIN),
            V3::Err(errno) => V3::err(errno),
        }
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }
}

// === tests pinning the v3 outcome shape end-to-end through ExecveTestFs ===

#[test]
fn execve_testfs_v3_lookup_round_trips_after_add_regular() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;

    let _setup = execve_setup();

    let bytes = minimal_elf_bytes();
    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);
    let file_id = fs.add_regular_with_bytes(root_id, b"init", &bytes);

    let guard = guard();
    assert_eq!(
        <ExecveTestFs as FsOps>::lookup(&*fs, root_id, b"init", &guard),
        V3::<_, NoProgress>::done(file_id)
    );
    assert_eq!(
        <ExecveTestFs as FsOps>::lookup(&*fs, root_id, b"missing", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn execve_testfs_v3_load_inode_meta_returns_directory_for_root() {
    use StepOutcome as V3;
    use tx_subsystems::vfs::FsOps;

    let _setup = execve_setup();

    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);

    let guard = guard();
    let meta = match <ExecveTestFs as FsOps>::load_inode_meta(&*fs, root_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Directory);
}

#[test]
fn execve_testfs_v3_read_link_returns_einval_for_regular() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;

    let _setup = execve_setup();

    let bytes = vec![0u8; 16];
    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);
    let file_id = fs.add_regular_with_bytes(root_id, b"f", &bytes);

    let guard = guard();
    assert_eq!(
        <ExecveTestFs as FsOps>::read_link(&*fs, file_id, &guard),
        V3::<Box<[u8]>, NoProgress>::err(V3Errno::EINVAL)
    );
}

#[test]
fn execve_testfs_v3_create_inode_returns_enosys() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;
    use tx_subsystems::vfs::structure::InodeMeta;

    let _setup = execve_setup();

    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);

    let guard = guard();
    let cred = Credential::root();
    assert_eq!(
        <ExecveTestFs as FsOps>::create_inode(&*fs, root_id, b"new", 0o644, &cred, &guard,),
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn execve_testfs_v3_fetch_page_returns_frame_for_regular() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::page_backed::FsPageBacking;

    let _setup = execve_setup();
    ensure_zero_frame_claimed();

    let bytes = vec![0xABu8; 64];
    let root_id = FsObjectId::new(2);
    let fs = ExecveTestFs::new(root_id);
    let file_id = fs.add_regular_with_bytes(root_id, b"f", &bytes);

    let guard = guard();
    match <ExecveTestFs as FsPageBacking>::fetch_page(&*fs, file_id, 0, &guard) {
        V3::Done(_frame) => {}
        other => panic!("fetch_page v3: {other:?}"),
    }
    // Misaligned offset → EINVAL.
    assert_eq!(
        <ExecveTestFs as FsPageBacking>::fetch_page(&*fs, file_id, 7, &guard),
        V3::<Frame, NoProgress>::err(V3Errno::EINVAL)
    );
    // Truncate / fsync / flush_page are synchronous Done(()).
    assert_eq!(
        <ExecveTestFs as FsPageBacking>::fsync_file(&*fs, file_id, &guard),
        V3::<(), NoProgress>::done(())
    );
}
