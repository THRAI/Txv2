# Bringup Filesystem Specs — v1

<!-- txdoc:05-FILESYSTEM-BRINGUP-FS-SPECS-V-1-1 -->

**Status.** Draft v1.

**Purpose.** Specify the three filesystem-facing components required by the bringup profile:

1. `TMPFS_v1` — the in-memory writable filesystem used as rootfs and `/tmp`.
2. `INITRAMFS_CPIO_v1` — the boot-time cpio `newc` unpacker that populates the tmpfs root.
3. `PROCFS_v1` — the minimal projected filesystem needed by static BusyBox bringup.

These specs are scoped to the bringup profile: static musl BusyBox, tmpfs root, minimal devfs/TTY, minimal procfs, and static ELF/static PIE exec. They are intentionally smaller than the later OSCOMP sdcard profile.

---

# Part I — TMPFS_v1

<!-- txdoc:BRINGUP-FS-PART-I-TMPFS-V1-1 -->

## 0. Purpose and scope

<!-- txdoc:BRINGUP-FS-PURPOSE-AND-SCOPE-1 -->

`TMPFS_v1` specifies the tmpfs backend used for:

```text
/                       rootfs during bringup
/tmp                    writable temporary directory
initramfs unpack target cpio newc -> tmpfs
```

tmpfs is a real `FsOps` backend. It is not a VFS special case and not a VM special case. It provides an in-memory namespace and metadata store. Regular file contents are stored in `PageContainerKind::Anon { swap_policy: Persistent }`, so read/write/mmap use the generic PageBacked machinery.

### 0.1 Goals

<!-- txdoc:BRINGUP-FS-GOALS-1 -->

```text
support root directory creation
support regular files backed by Anon/Persistent PageContainers
support directories with name -> FsObjectId maps
support symlinks with inline target bytes
support cpio unpack for static BusyBox rootfs
support ordinary VFS operations: lookup, create, mkdir, unlink, rmdir, rename, readdir, symlink, readlink
support unlink-open lifetime through RNode/PageContainer retention
```

### 0.2 Non-goals

<!-- txdoc:BRINGUP-FS-NON-GOALS-1 -->

```text
swap-backed eviction
memory-pressure reclaim beyond object teardown
xattrs / ACLs / SELinux labels
Linux tmpfs mount-option completeness
huge pages
advanced fallocate / punch-hole
inotify/fsnotify
persistent storage
```

---

## 1. Architectural position

<!-- txdoc:BRINGUP-FS-ARCHITECTURAL-POSITION-1 -->

```text
MountPayload(tmpfs)
    owns TmpfsMountPayload
    exposes FsOps and FsPageBacking trait objects

VFS
    owns path walking, DEntry, RNode, OpenFile, fd table
    calls tmpfs FsOps by fs_object_id

PageBacked
    owns PageContainer read/write/mmap behavior
    stores regular file contents in Anon/Persistent PCs

tmpfs
    owns fs_object_id allocation
    owns TmpfsNode metadata
    owns directory children maps
    owns symlink target bytes
    owns references to regular-file PageContainers
```

Boundary rule:

```text
TMPFS-BDY-1:
    tmpfs never owns DEntry, RNode, OpenFile, or mount topology.

TMPFS-BDY-2:
    VFS never inspects tmpfs internal node maps directly.

TMPFS-BDY-3:
    file content operations use PageBacked; tmpfs does not implement read/write loops itself.
```

---

## 2. Data structures

<!-- txdoc:BRINGUP-FS-DATA-STRUCTURES-1 -->

### 2.1 `TmpfsMountPayload`

<!-- txdoc:BRINGUP-FS-TMPFSMOUNTPAYLOAD-1 -->

```rust
pub struct TmpfsMountPayload {
    pub meta: SlotMeta,

    /// Monotonic allocator for fs object ids. 1 is reserved for root.
    pub next_ino: AtomicU64,

    /// Authoritative tmpfs object table.
    pub nodes: RwLock<BTreeMap<FsObjectId, TmpfsNode>>,

    /// Root object id. Always FsObjectId(1) in v1.
    pub root: FsObjectId,

    /// Mount options captured at mount time.
    pub options: TmpfsOptions,

    /// Optional accounting. v1 may enforce or only observe these.
    pub usage: TmpfsUsage,
}
```

### 2.2 `TmpfsOptions`

<!-- txdoc:BRINGUP-FS-TMPFSOPTIONS-1 -->

```rust
pub struct TmpfsOptions {
    /// Mode for the root directory when the mount is created.
    pub root_mode: u16,

    /// Optional byte limit for file contents. None means unlimited except physical memory.
    pub size_limit: Option<u64>,

    /// Optional inode limit. None means unlimited except kernel memory.
    pub inode_limit: Option<u64>,
}
```

Default policies:

```text
root tmpfs: root_mode = 0755
/tmp tmpfs: root_mode = 01777
size_limit: None in bringup
inode_limit: None in bringup
```

### 2.3 `TmpfsUsage`

<!-- txdoc:BRINGUP-FS-TMPFSUSAGE-1 -->

```rust
pub struct TmpfsUsage {
    pub allocated_nodes: AtomicU64,
    pub logical_bytes: AtomicU64,
}
```

`logical_bytes` tracks file sizes, not necessarily resident frame bytes. Resident frame accounting belongs to PageBacked/frame allocator.

### 2.4 `TmpfsNode`

<!-- txdoc:BRINGUP-FS-TMPFSNODE-1 -->

```rust
pub struct TmpfsNode {
    pub id: FsObjectId,
    pub meta: TmpfsInodeMeta,
    pub kind: TmpfsNodeKind,
}

pub struct TmpfsInodeMeta {
    pub mode: u16,
    pub uid: Uid,
    pub gid: Gid,
    pub size: AtomicU64,
    pub nlinks: AtomicU32,
    pub atime: AtomicTime,
    pub mtime: AtomicTime,
    pub ctime: AtomicTime,
}

pub enum TmpfsNodeKind {
    Directory {
        children: BTreeMap<Name, FsObjectId>,
    },

    Regular {
        pc: Cap<PageContainer>,
    },

    Symlink {
        target: Vec<u8>,
    },
}
```

### 2.5 FsObjectId convention

<!-- txdoc:BRINGUP-FS-FSOBJECTID-CONVENTION-1 -->

`FsObjectId` is tmpfs-local and allocated monotonically:

```text
root: FsObjectId(1)
next: 2, 3, 4, ...
```

Ids are not reused in v1. This simplifies stale-reference defense and `/proc` rendering during bringup.

---

## 3. Root creation

<!-- txdoc:BRINGUP-FS-ROOT-CREATION-1 -->

`tmpfs::mount_init(options)` creates:

```text
TmpfsMountPayload
root TmpfsNode { id = 1, kind = Directory, mode = options.root_mode }
```

Then returns the filesystem driver output used by Mount:

```rust
pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}
```

Root node invariants:

```text
TMPFS-ROOT-1:
    root exists for the lifetime of TmpfsMountPayload.

TMPFS-ROOT-2:
    root.nlinks is at least 2.

TMPFS-ROOT-3:
    root may not be removed by rmdir.
```

---

## 4. FsOps implementation

<!-- txdoc:BRINGUP-FS-FSOPS-IMPLEMENTATION-1 -->

### 4.1 `lookup`

<!-- txdoc:BRINGUP-FS-LOOKUP-1 -->

```rust
fn lookup(
    &self,
    parent: FsObjectId,
    name: &[u8],
    guard: &Guard,
) -> StepOutcome<FsObjectId>;
```

Rules:

```text
if parent does not exist -> ENOENT
if parent is not directory -> ENOTDIR
if name is empty -> ENOENT
if name contains '/' or NUL -> EINVAL
if child exists -> Done(child_id)
else -> ENOENT
```

`.` and `..` are handled by VFS walker, not by tmpfs lookup.

### 4.2 `load_inode_meta`

<!-- txdoc:BRINGUP-FS-LOAD-INODE-META-1 -->

```rust
fn load_inode_meta(
    &self,
    id: FsObjectId,
    guard: &Guard,
) -> StepOutcome<InodeMeta>;
```

Returns decoded metadata for VFS/RNode creation.

For directories:

```text
mode includes S_IFDIR
size may be 0 or implementation-defined directory pseudo-size
nlinks tracks 2 + child directories if maintained
```

For regular files:

```text
mode includes S_IFREG
size is TmpfsInodeMeta.size
backing is PageBacked(pc)
```

For symlinks:

```text
mode includes S_IFLNK
size is target.len()
```

### 4.3 `create_inode`

<!-- txdoc:BRINGUP-FS-CREATE-INODE-1 -->

```rust
fn create_inode(
    &self,
    parent: FsObjectId,
    name: &[u8],
    mode: u16,
    cred: &Credential,
    guard: &Guard,
) -> StepOutcome<(FsObjectId, InodeMeta)>;
```

Creates a regular file.

Commit behavior:

```text
1. validate parent is existing directory
2. validate name absent
3. reserve fs_object_id
4. reserve TmpfsNode
5. allocate PageContainerKind::Anon { Persistent }, size = 0
6. insert node into nodes map
7. insert name -> id into parent.children
8. return id and metadata
```

The name insertion is the namespace visibility point within tmpfs.

Failure before step 7 leaves no visible directory entry.

Errors:

```text
ENOENT       parent missing
ENOTDIR      parent not directory
EEXIST       name exists
ENAMETOOLONG name too long
ENOSPC       inode or size limit exceeded
ENOMEM       allocation failure
```

### 4.4 `mkdir`

<!-- txdoc:BRINGUP-FS-MKDIR-1 -->

```rust
fn mkdir(
    &self,
    parent: FsObjectId,
    name: &[u8],
    mode: u16,
    cred: &Credential,
    guard: &Guard,
) -> StepOutcome<(FsObjectId, InodeMeta)>;
```

Creates a directory node.

Directory link policy:

```text
new directory nlinks = 2
parent directory nlinks += 1 if link counts are maintained
```

v1 may maintain approximate link counts sufficient for stat. Correct removal checks use the children map, not nlinks.

### 4.5 `symlink`

<!-- txdoc:BRINGUP-FS-SYMLINK-1 -->

```rust
fn symlink(
    &self,
    parent: FsObjectId,
    name: &[u8],
    target: &[u8],
    cred: &Credential,
    guard: &Guard,
) -> StepOutcome<(FsObjectId, InodeMeta)>;
```

Creates a symlink node with inline target bytes.

Rules:

```text
target may be relative or absolute
target bytes are not interpreted by tmpfs
empty target -> EINVAL
excessively long target -> ENAMETOOLONG
```

### 4.6 `readlink`

<!-- txdoc:BRINGUP-FS-READLINK-1 -->

```rust
fn readlink(
    &self,
    id: FsObjectId,
    guard: &Guard,
) -> StepOutcome<Vec<u8>>;
```

Returns a copy of the inline symlink target.

If VFS already stores symlink payload in RNode metadata, this may be folded into `load_inode_meta`; the tmpfs spec allows either implementation as long as symlink target bytes remain tmpfs-owned.

### 4.7 `unlink`

<!-- txdoc:BRINGUP-FS-UNLINK-1 -->

```rust
fn unlink(
    &self,
    parent: FsObjectId,
    name: &[u8],
    target: FsObjectId,
    guard: &Guard,
) -> StepOutcome<()>;
```

Rules:

```text
parent must be existing directory
name must exist and map to target
target must not be directory
remove name from parent.children
decrement target.nlinks
if target.nlinks reaches 0, mark unlinked
actual node destruction happens through destroy_inode when VFS/RNode lifecycle permits
```

Errors:

```text
ENOENT   missing parent or child
ENOTDIR  parent not directory
EISDIR   target is directory
```

### 4.8 `rmdir`

<!-- txdoc:BRINGUP-FS-RMDIR-1 -->

```rust
fn rmdir(
    &self,
    parent: FsObjectId,
    name: &[u8],
    target: FsObjectId,
    guard: &Guard,
) -> StepOutcome<()>;
```

Rules:

```text
target must be directory
target must be empty
target must not be root
remove name from parent.children
decrement parent directory nlinks if maintained
mark target unlinked
actual destruction happens through destroy_inode when VFS/RNode lifecycle permits
```

Errors:

```text
ENOENT     missing entry
ENOTDIR    target not directory
ENOTEMPTY  target directory has children
EINVAL     target is root
```

Active mountpoint rejection is handled by VFS/Mount before tmpfs rmdir is called.

### 4.9 `rename`

<!-- txdoc:BRINGUP-FS-RENAME-1 -->

```rust
fn rename(
    &self,
    old_parent: FsObjectId,
    old_name: &[u8],
    new_parent: FsObjectId,
    new_name: &[u8],
    guard: &Guard,
) -> StepOutcome<RenameResult>;
```

v1 supports:

```text
file -> absent
file -> existing file overwrite
directory -> absent
directory -> empty directory overwrite
same-directory rename
cross-directory rename within the same tmpfs mount
```

v1 rejects or defers:

```text
renameat2 flags
RENAME_EXCHANGE
RENAME_NOREPLACE
whiteout
cross-mount rename, rejected by VFS as EXDEV
```

Required checks:

```text
old parent and new parent exist and are directories
old name exists
cannot rename root
cannot move a directory into its own subtree
file over directory -> EISDIR or ENOTDIR according to syscall wrapper policy
directory over file -> ENOTDIR or EISDIR according to syscall wrapper policy
non-empty target directory -> ENOTEMPTY
```

Lock ordering:

```text
If two directories are involved, tmpfs locks directory nodes in FsObjectId order.
If old_parent == new_parent, acquire once.
```

Visibility:

```text
The rename commit updates directory maps under tmpfs internal lock.
Observers see either the old binding or the new binding; they must not see a state where the source disappeared and the target is not yet installed.
```

Implementation may use a coarse tmpfs-wide write lock in v1.

### 4.10 `readdir`

<!-- txdoc:BRINGUP-FS-READDIR-1 -->

```rust
fn readdir(
    &self,
    parent: FsObjectId,
    cursor: DirCursor,
    guard: &Guard,
) -> StepOutcome<Option<(DirEntry, DirCursor)>>;
```

Rules:

```text
parent must be directory
cursor is tmpfs-local and opaque to userspace
entries include . and .. only if VFS expects backend to emit them; otherwise VFS synthesizes them
ordinary entries come from children map
```

Recommended v1 policy:

```text
VFS synthesizes . and ..
tmpfs readdir emits only real children
```

### 4.11 `destroy_inode`

<!-- txdoc:BRINGUP-FS-DESTROY-INODE-1 -->

```rust
fn destroy_inode(
    &self,
    id: FsObjectId,
    guard: &Guard,
) -> StepOutcome<()>;
```

Called by VFS/RNode lifecycle when no live namespace links and no operational users remain.

Rules:

```text
if id is root -> EINVAL or no-op unreachable
if node.nlinks > 0 -> Done(()) or EBUSY according to VFS contract
remove id from nodes map
if regular: drop PageContainer Cap
if symlink: drop target bytes
if directory: must be empty
update usage counters
```

---

## 5. FsPageBacking implementation

<!-- txdoc:BRINGUP-FS-FSPAGEBACKING-IMPLEMENTATION-1 -->

Regular tmpfs file contents are held in `PageContainerKind::Anon { Persistent }`.

Therefore tmpfs does not perform disk fetch or disk flush.

```rust
impl FsPageBacking for TmpfsMountPayload {
    fn fetch_page(&self, id: FsObjectId, offset: u64, guard: &Guard)
        -> StepOutcome<Frame>
    {
        // Normally not used for Anon/Persistent PCs.
        // If reached through a generic path, zero-fill is correct.
        Done(frame::alloc_zeroed()?)
    }

    fn flush_page(&self, id: FsObjectId, offset: u64, frame: &Frame, guard: &Guard)
        -> StepOutcome<()>
    {
        Done(())
    }

    fn truncate(&self, id: FsObjectId, new_size: u64, guard: &Guard)
        -> StepOutcome<()>
    {
        // Update TmpfsNode.meta.size; PageBacked invalidates pages beyond size.
        Done(())
    }

    fn fsync(&self, id: FsObjectId, guard: &Guard)
        -> StepOutcome<()>
    {
        Done(())
    }
}
```

Normative rule:

```text
TMPFS-FS-BACKING-1:
    tmpfs regular-file data must not require backing-storage I/O.
```

---

## 6. PageContainer and RNode creation

<!-- txdoc:BRINGUP-FS-PAGECONTAINER-RNODE-CREATION-1 -->

For a regular tmpfs file:

```text
TmpfsNodeKind::Regular holds Cap<PageContainer>
PageContainer.kind = Anon { swap_policy: Persistent }
RNodeBacking = PageBacked { pc }
```

Creation path:

```text
1. tmpfs.create_inode allocates TmpfsNode and PageContainer.
2. tmpfs returns FsObjectId + InodeMeta.
3. VFS creates or finds RNode for FsObjectId.
4. VFS sets RNodeBacking::PageBacked { pc } using tmpfs-provided PC.
5. VFS installs DEntry.
```

`TMPFS_v1` needs a small backend-to-VFS handoff for the PC. Two acceptable shapes:

### Option A — InodeMeta extension

<!-- txdoc:BRINGUP-FS-OPTION-INODEMETA-EXTENSION-1 -->

```rust
pub struct InodeMeta {
    ...
    pub tmpfs_pc: Option<Cap<PageContainer>>, // internal extension, not generic
}
```

This is not preferred because it pollutes generic metadata.

### Option B — backend hook

<!-- txdoc:BRINGUP-FS-OPTION-B-BACKEND-HOOK-1 -->

```rust
trait FsOps {
    fn backing_for(&self, id: FsObjectId, guard: &Guard)
        -> StepOutcome<RNodeBackingInit>;
}

pub enum RNodeBackingInit {
    PageBacked { pc: Cap<PageContainer> },
    StructBacked { payload: StructPayload },
    Projected { schema: &'static dyn ProjectionSchema, key: ProjectionKey },
}
```

Preferred v1 rule:

```text
VFS asks the backend for RNodeBackingInit when materializing an RNode.
tmpfs returns PageBacked { pc } for regular files, symlink metadata for symlinks, and directory metadata for directories.
```

---

## 7. Lifetime and unlink-open behavior

<!-- txdoc:BRINGUP-FS-LIFETIME-UNLINK-OPEN-BEHAVIOR-1 -->

### 7.1 Namespace link vs payload lifetime

<!-- txdoc:BRINGUP-FS-NAMESPACE-LINK-PAYLOAD-LIFETIME-1 -->

`unlink` and `rmdir` remove directory names. They do not directly destroy live RNodes or PageContainers.

```text
name removed -> node.nlinks decremented
open file exists -> RNode and PageContainer stay alive
last RNode/OpenFile/PageContainer ref drops -> VFS calls destroy_inode
```

### 7.2 Regular file after unlink

<!-- txdoc:BRINGUP-FS-REGULAR-FILE-AFTER-UNLINK-1 -->

If a regular file is opened and then unlinked:

```text
fd read/write continues to use the same RNode
RNode holds PageContainer
PageContainer holds resident frames
node is removed from directory namespace
node table entry may remain until destroy_inode
```

This matches the architecture's payload/namespace split.

---

## 8. Mount options

<!-- txdoc:BRINGUP-FS-MOUNT-OPTIONS-1 -->

Mount source for tmpfs:

```rust
MountSource::None
MountSource::MagicName("tmpfs")
```

`MountOptions::tmpfs` carries `TmpfsOptions`.

Supported v1 options:

```text
mode=...
size=...       optional; may be parsed but not enforced in early bringup
nr_inodes=...  optional; may be parsed but not enforced in early bringup
```

Unsupported options are ignored or rejected according to mount syscall policy. For bringup, kernel-created tmpfs root passes structured options directly and does not parse string options.

---

## 9. Technical debt

<!-- txdoc:BRINGUP-FS-TECHNICAL-DEBT-1 -->

```text
TD-TMPFS-1: no swap or memory-pressure eviction
TD-TMPFS-2: no xattrs / ACLs / security labels
TD-TMPFS-3: incomplete Linux tmpfs mount options
TD-TMPFS-4: no advanced fallocate / punch-hole
TD-TMPFS-5: approximate timestamp and link-count behavior acceptable during bringup
TD-TMPFS-6: optional cpio hardlink support deferred unless image requires it
```

---

# Part II — INITRAMFS_CPIO_v1

<!-- txdoc:BRINGUP-FS-PART-II-INITRAMFS-CPIO-V1-1 -->

## 0. Purpose and scope

<!-- txdoc:BRINGUP-FS-PURPOSE-AND-SCOPE-2 -->

`INITRAMFS_CPIO_v1` specifies the boot-time unpacker for a cpio `newc` archive stored in `BootInfo.initrd`. The unpacker populates the tmpfs root filesystem through VFS operations.

The unpacker is a boot script. It is not part of tmpfs and does not bypass VFS.

---

## 1. Inputs and outputs

<!-- txdoc:BRINGUP-FS-INPUTS-AND-OUTPUTS-1 -->

Input:

```rust
pub struct InitrdRange {
    pub start: PhysAddr,
    pub len: usize,
}
```

Output:

```text
A populated tmpfs root filesystem.
```

Top-level function:

```rust
pub fn unpack_newc(
    initrd: InitrdRange,
    root_ctx: &KernelRootCtx,
) -> StepOutcome<()>;
```

`KernelRootCtx` contains:

```rust
pub struct KernelRootCtx {
    pub mnt_ns: Cap<MountNamespace>,
    pub root_mount: Cap<MountIdentity>,
    pub root_dentry: Cap<DEntry>,
    pub cred: CredentialSnapshot, // root/kernel credential
}
```

---

## 2. Format support

<!-- txdoc:BRINGUP-FS-FORMAT-SUPPORT-1 -->

Only cpio `newc` is supported in v1.

Magic:

```text
070701
```

Unsupported formats:

```text
old ASCII cpio
crc cpio 070702
tar
compressed archives directly; decompression is a separate boot-loader or pre-unpack step
```

If the archive is compressed, the platform loader or a separate decompressor must provide an uncompressed `newc` buffer before `unpack_newc` runs.

---

## 3. Header fields

<!-- txdoc:BRINGUP-FS-HEADER-FIELDS-1 -->

`newc` header fields are ASCII hexadecimal:

```text
c_magic
c_ino
c_mode
c_uid
c_gid
c_nlink
c_mtime
c_filesize
c_devmajor
c_devminor
c_rdevmajor
c_rdevminor
c_namesize
c_check
```

v1 consumes:

```text
c_mode
c_uid
c_gid
c_nlink
c_filesize
c_namesize
c_mtime optional
c_ino optional for hardlink tracking
```

v1 ignores:

```text
c_check
c_devmajor/c_devminor except optional hardlink key
c_rdevmajor/c_rdevminor unless device-node support is later added
```

---

## 4. Alignment and parsing

<!-- txdoc:BRINGUP-FS-ALIGNMENT-AND-PARSING-1 -->

Rules:

```text
header size is 110 bytes
filename follows header and includes trailing NUL
file payload follows filename
header + filename is 4-byte aligned
file payload is 4-byte aligned
archive ends at entry name "TRAILER!!!"
```

Parsing errors are fatal to boot:

```text
bad magic
short header
invalid hex field
namesize == 0
filename missing trailing NUL
file payload exceeds initrd range
alignment overflow
missing TRAILER!!!
```

---

## 5. Path policy

<!-- txdoc:BRINGUP-FS-PATH-POLICY-1 -->

Every cpio path is normalized before use.

Rules:

```text
leading "./" is stripped
leading "/" is rejected or stripped according to build policy; v1 strips it
empty path is rejected
path containing NUL before final terminator is rejected
path containing ".." component is rejected
path longer than PATH_MAX is rejected
component longer than NAME_MAX is rejected
```

The unpacker writes relative to the tmpfs root.

Examples:

```text
"bin/busybox" -> "/bin/busybox"
"./etc/profile" -> "/etc/profile"
"/init" -> "/init" after leading slash strip
"../bad" -> rejected
```

---

## 6. Supported entry kinds

<!-- txdoc:BRINGUP-FS-SUPPORTED-ENTRY-KINDS-1 -->

### 6.1 Directory

<!-- txdoc:BRINGUP-FS-DIRECTORY-1 -->

Condition:

```text
(c_mode & S_IFMT) == S_IFDIR
```

Action:

```rust
vfs::kernel_mkdir(path, mode & 0o7777, uid, gid, root_ctx)
```

If the directory already exists, v1 may treat it as success if it is a directory.

### 6.2 Regular file

<!-- txdoc:BRINGUP-FS-REGULAR-FILE-1 -->

Condition:

```text
(c_mode & S_IFMT) == S_IFREG
```

Action:

```text
create or truncate file
write c_filesize bytes in bounded chunks
apply mode/uid/gid metadata
```

Required helper shape:

```rust
pub fn kernel_write_file_from_slice(
    path: &Path,
    bytes: &[u8],
    meta: CpioMeta,
    ctx: &KernelRootCtx,
) -> StepOutcome<()>;
```

Chunking:

```text
writes are chunked to avoid long unbounded memcpy loops
recommended chunk size: PAGE_SIZE or 16 KiB
```

### 6.3 Symlink

<!-- txdoc:BRINGUP-FS-SYMLINK-2 -->

Condition:

```text
(c_mode & S_IFMT) == S_IFLNK
```

Action:

```text
read file payload bytes as symlink target
target is not required to be NUL-terminated
create symlink at path
apply uid/gid/mode metadata if supported
```

### 6.4 Device nodes

<!-- txdoc:BRINGUP-FS-DEVICE-NODES-1 -->

Conditions:

```text
S_IFCHR
S_IFBLK
```

v1 behavior:

```text
skip with warning or reject according to strictness option
```

Bringup policy:

```text
skip device nodes because devfs owns /dev
```

### 6.5 FIFO/socket

<!-- txdoc:BRINGUP-FS-FIFO-SOCKET-1 -->

v1 behavior:

```text
skip or reject
```

Default bringup policy:

```text
reject FIFO/socket if present in required image
```

---

## 7. Hardlink policy

<!-- txdoc:BRINGUP-FS-HARDLINK-POLICY-1 -->

cpio `newc` represents hardlinks through repeated `(c_devmajor, c_devminor, c_ino)` with `c_nlink > 1`.

v1 policy options:

### Option A — defer hardlinks

<!-- txdoc:BRINGUP-FS-OPTION-DEFER-HARDLINKS-1 -->

```text
If c_nlink > 1 and c_filesize == 0 for a later hardlink entry, return ENOTSUP.
The bringup image builder must use symlinks or duplicate files instead.
```

### Option B — support simple hardlinks

<!-- txdoc:BRINGUP-FS-OPTION-B-SUPPORT-SIMPLE-HARDLINKS-1 -->

Maintain during unpack:

```rust
BTreeMap<CpioHardlinkKey, Path>
```

On first regular file with `c_nlink > 1`:

```text
create file normally
record key -> path
```

On subsequent entry with same key:

```text
vfs::kernel_link(existing_path, new_path)
```

`BRINGUP_v1` permits either. Recommended initial implementation is Option A unless the generated BusyBox image uses hardlinks.

---

## 8. Ordering policy

<!-- txdoc:BRINGUP-FS-ORDERING-POLICY-1 -->

The image builder should emit parent directories before children.

If parent is missing:

```text
v1 returns ENOENT
```

The unpacker does not implicitly create missing parents in v1.

Rationale:

```text
implicit parent creation hides image bugs
cpio generators can easily emit directories first
```

---

## 9. Metadata policy

<!-- txdoc:BRINGUP-FS-METADATA-POLICY-1 -->

Required:

```text
mode
uid
gid
size
```

Optional:

```text
mtime
ctime
atime
```

For bringup, timestamps may be approximate or fixed.

`chmod` / `chown` may be implemented as direct VFS metadata updates under kernel credential after object creation.

---

## 10. Error policy

<!-- txdoc:BRINGUP-FS-ERROR-POLICY-1 -->

Fatal errors abort boot:

```text
malformed archive
unsupported required file type
path escapes root
ENOMEM
write failure
metadata update failure
```

Non-fatal warnings:

```text
skipped device node under /dev
ignored mtime
ignored cpio checksum
```

Strict mode may turn warnings into fatal errors.

---

## 11. Security and robustness

<!-- txdoc:BRINGUP-FS-SECURITY-AND-ROBUSTNESS-1 -->

The initramfs is trusted input in bringup, but the parser must still be memory safe.

Required checks:

```text
all offset arithmetic checked for overflow
all payload ranges checked against initrd length
all names checked for termination and length
all paths normalized before VFS use
no direct writes outside tmpfs root
```

---

## 12. Technical debt

<!-- txdoc:BRINGUP-FS-TECHNICAL-DEBT-2 -->

```text
TD-CPIO-1: no compressed initramfs inside kernel unpacker
TD-CPIO-2: hardlinks optional/deferred
TD-CPIO-3: device nodes skipped; devfs owns /dev
TD-CPIO-4: incomplete timestamp fidelity
TD-CPIO-5: no xattrs/ACLs/security labels
```

---

# Part III — PROCFS_v1

<!-- txdoc:BRINGUP-FS-PART-III-PROCFS-V1-1 -->

## 0. Purpose and scope

<!-- txdoc:BRINGUP-FS-PURPOSE-AND-SCOPE-3 -->

`PROCFS_v1` specifies the minimal projected filesystem mounted at `/proc` during bringup.

procfs is not PageBacked. It stores no persistent file contents. Every readable file is a projection over current kernel state.

### 0.1 Goals

<!-- txdoc:BRINGUP-FS-GOALS-2 -->

```text
support /proc/mounts
support /proc/self
support /proc/self/fd
support /proc/self/exe
support /proc/meminfo
support /proc/cpuinfo
support /proc/uptime
provide enough Linux-like text for BusyBox smoke tests
```

### 0.2 Non-goals

<!-- txdoc:BRINGUP-FS-NON-GOALS-2 -->

```text
complete Linux procfs
/proc/<pid>/stat exact compatibility
/proc/sys
/proc/net
/proc/filesystems completeness
ptrace-sensitive visibility rules
hidepid mount options
per-namespace procfs isolation beyond current MountNamespace/PidNamespace view
```

---

## 1. Architectural position

<!-- txdoc:BRINGUP-FS-ARCHITECTURAL-POSITION-2 -->

procfs is a filesystem backend that returns Projected RNodes.

```text
MountPayload(procfs)
    stores ProcfsMountPayload
    exposes FsOps

VFS
    owns DEntry/RNode/OpenFile/path walking

procfs
    owns projection schema catalog
    maps path components to ProjectionKey
    renders text on read

Process/Mount/VM/FD/etc.
    own the state being projected

Namespace view
    ProcfsMountPayload carries the pid/mount namespace lenses used for
    path lookup and rendering; see NAMESPACE_VIEW_v1
```

Boundary rules:

```text
PROCFS-BDY-1:
    procfs does not own Process, Mount, VM, FD, or Signal state.

PROCFS-BDY-2:
    procfs does not cache projected file contents as authority.

PROCFS-BDY-3:
    procfs may synthesize DEntry/RNode identities, but projected truth is re-observed on read.
```

---

## 2. Data structures

<!-- txdoc:BRINGUP-FS-DATA-STRUCTURES-2 -->

### 2.1 `ProcfsMountPayload`

<!-- txdoc:BRINGUP-FS-PROCFSMOUNTPAYLOAD-1 -->

```rust
pub struct ProcfsMountPayload {
    pub meta: SlotMeta,

    /// Viewpoint for pid lookup/rendering. Paths like /proc/<pid> resolve
    /// through PidNamespace.numbers -> PidName -> canonical process identity.
    pub pid_ns: Cap<PidNamespace>,

    /// Viewpoint for /proc/mounts.
    pub mount_ns: Cap<MountNamespace>,

    /// Static projection schema registry.
    pub schemas: &'static ProcfsSchemaRegistry,
}
```

### 2.2 Projection key

<!-- txdoc:BRINGUP-FS-PROJECTION-KEY-1 -->

```rust
pub enum ProcfsKey {
    Root,
    SelfLink,

    PidDir { pid: Pid },
    PidStatus { pid: Pid },
    PidCmdline { pid: Pid },
    PidExe { pid: Pid },
    PidFdDir { pid: Pid },
    PidFdEntry { pid: Pid, fd: FdNum },

    Mounts,
    Meminfo,
    Cpuinfo,
    Uptime,
}
```

### 2.3 RNode backing

<!-- txdoc:BRINGUP-FS-RNODE-BACKING-1 -->

procfs uses:

```rust
RNodeBacking::Projected {
    schema: &'static dyn ProjectionSchema,
    key: ProjectionKey,
}
```

Directories may be represented as projected directories whose `readdir` is generated from the schema.

---

## 3. FsOps shape

<!-- txdoc:BRINGUP-FS-FSOPS-SHAPE-1 -->

### 3.1 `lookup`

<!-- txdoc:BRINGUP-FS-LOOKUP-2 -->

`lookup(parent, name)` maps procfs path components to `ProcfsKey`.

Required tree:

```text
/proc
├── self -> /proc/<current-pid>
├── mounts
├── meminfo
├── cpuinfo
├── uptime
└── <pid>/
    ├── status
    ├── cmdline
    ├── exe
    └── fd/
        ├── 0
        ├── 1
        └── 2
```

v1 may only materialize pid directories for live processes.

Lookup rules:

```text
root + "self"      -> SelfLink
root + "mounts"    -> Mounts
root + "meminfo"   -> Meminfo
root + "cpuinfo"   -> Cpuinfo
root + "uptime"    -> Uptime
root + decimal pid  -> PidDir(pid) if pid is addressable
PidDir + "status"  -> PidStatus(pid)
PidDir + "cmdline" -> PidCmdline(pid)
PidDir + "exe"     -> PidExe(pid)
PidDir + "fd"      -> PidFdDir(pid)
PidFdDir + decimal fd -> PidFdEntry(pid, fd) if fd exists
```

### 3.2 `load_inode_meta`

<!-- txdoc:BRINGUP-FS-LOAD-INODE-META-2 -->

Metadata is synthesized.

Recommended modes:

| key | mode |
|---|---|
| root | `S_IFDIR | 0555` |
| pid dir | `S_IFDIR | 0555` |
| fd dir | `S_IFDIR | 0500` or `0555` in v1 |
| self | `S_IFLNK | 0777` |
| exe | `S_IFLNK | 0777` |
| fd entry | `S_IFLNK | 0777` |
| text file | `S_IFREG | 0444` |

Size may be:

```text
0 for dynamic projected files
estimated render length if cheap
```

### 3.3 `readdir`

<!-- txdoc:BRINGUP-FS-READDIR-2 -->

Required directory entries:

`/proc`:

```text
self
mounts
meminfo
cpuinfo
uptime
<pid entries from pid_ns.numbers whose PidName kind is Process>
```

`/proc/<pid>`:

```text
status
cmdline
exe
fd
```

`/proc/<pid>/fd`:

```text
one entry per live fd in the canonical process fd table, rendered through the procfs view
```

VFS may synthesize `.` and `..`; procfs emits only real entries.

### 3.4 Mutations

<!-- txdoc:BRINGUP-FS-MUTATIONS-1 -->

procfs is read-only in v1.

```text
create -> EROFS
mkdir  -> EROFS
unlink -> EROFS
rmdir  -> EROFS
rename -> EROFS
link   -> EROFS
symlink -> EROFS
truncate -> EINVAL or EROFS
```

---

## 4. Projection read model

<!-- txdoc:BRINGUP-FS-PROJECTION-READ-MODEL-1 -->

Projected file reads use a render buffer.

```rust
pub trait ProjectionSchema {
    fn render(
        &self,
        key: &ProjectionKey,
        ctx: &ProjectionReadCtx,
        out: &mut RenderBuffer,
    ) -> Result<(), Errno>;
}
```

Read behavior:

```text
1. observe current target state under guard
2. render complete file contents into kernel buffer
3. serve requested [offset, offset+len) slice to user
4. discard buffer after read
```

v1 may cap render size:

```text
PROCFS_MAX_RENDER = 64 KiB
```

If rendered output exceeds the cap:

```text
return EOVERFLOW or truncate according to file-specific policy
```

Recommended v1 policy: return `EOVERFLOW` for non-stub files.

---

## 5. Required projections

<!-- txdoc:BRINGUP-FS-REQUIRED-PROJECTIONS-1 -->

### 5.1 `/proc/mounts`

<!-- txdoc:BRINGUP-FS-PROC-MOUNTS-1 -->

Source:

```text
current procfs MountPayload.mount_ns.all_mounts
```

Format:

```text
source target fstype options 0 0
```

Minimum options rendering:

```text
rw or ro
nosuid if set
nodev if set
noexec if set
noatime if set
```

Example:

```text
tmpfs / tmpfs rw 0 0
devfs /dev devfs rw 0 0
proc /proc proc rw 0 0
```

Rules:

```text
/proc/mounts is projection, not a shadow mount table
render observes all_mounts under guard
if a mount is concurrently detached, either old or new list is acceptable
```

### 5.2 `/proc/self`

<!-- txdoc:BRINGUP-FS-PROC-SELF-1 -->

`/proc/self` is a symlink-like projection.

Readlink behavior:

```text
current process pid = N
readlink("/proc/self") -> "N"
```

It is viewpoint-dependent: it resolves relative to the calling thread's active
`nsproxy.pid_ns`, not merely the procfs mount's stored pid namespace. Ordinary
`/proc/<pid>` path components resolve through the procfs mount view.

### 5.3 `/proc/<pid>/status`

<!-- txdoc:BRINGUP-FS-PROC-STATUS-1 -->

Minimum v1 fields:

```text
Name:	<comm>
State:	<R|S|Z>
Pid:	<pid>
PPid:	<ppid>
Uid:	<ruid>	<euid>	<suid>	<fsuid>
Gid:	<rgid>	<egid>	<sgid>	<fsgid>
Threads:	<n>
```

Stub or optional fields may be omitted during bringup.

If pid is not addressable:

```text
ENOENT
```

### 5.4 `/proc/<pid>/cmdline`

<!-- txdoc:BRINGUP-FS-PROC-CMDLINE-1 -->

Source:

```text
ProcessPayload.cmdline snapshot recorded at exec commit
```

Format:

```text
argv strings separated by NUL bytes
final NUL after last arg if argv non-empty
```

If process has no cmdline:

```text
empty file
```

### 5.5 `/proc/<pid>/exe`

<!-- txdoc:BRINGUP-FS-PROC-EXE-1 -->

Source:

```rust
ProcessPayload.exe_file: Option<ExecutableImageRef>
```

```rust
pub struct ExecutableImageRef {
    pub mount: Cap<MountIdentity>,
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
}
```

Behavior:

```text
readlink returns best-effort current path to executable
if unlinked, may append " (deleted)" or return best-effort object marker
if unavailable, ENOENT
```

For bringup, best-effort path rendering is acceptable.

### 5.6 `/proc/<pid>/fd`

<!-- txdoc:BRINGUP-FS-PROC-FD-1 -->

Directory projection over process fd table.

Entries:

```text
one decimal filename per live fd
```

If pid not addressable:

```text
ENOENT
```

Permission model:

```text
v1 allows same-uid or root; bringup may allow all reads
```

### 5.7 `/proc/<pid>/fd/<n>`

<!-- txdoc:BRINGUP-FS-PROC-FD-2 -->

Symlink-like projection over fd entry.

Render examples:

```text
/dev/console
/tmp/file
pipe:[id]
socket:[id]
anon_inode:[eventfd]
```

Bringup minimum:

```text
fd 0/1/2 for pid1 render as /dev/console
regular files render best-effort path
unknown backing renders anon_inode:[unknown]
```

### 5.8 `/proc/meminfo`

<!-- txdoc:BRINGUP-FS-PROC-MEMINFO-1 -->

Stub acceptable in bringup.

Minimum format:

```text
MemTotal:        <kB> kB
MemFree:         <kB> kB
MemAvailable:   <kB> kB
Buffers:              0 kB
Cached:               0 kB
```

Values may be approximate from frame allocator.

### 5.9 `/proc/cpuinfo`

<!-- txdoc:BRINGUP-FS-PROC-CPUINFO-1 -->

Stub acceptable in bringup.

RISC-V example:

```text
processor	: 0
hart		: 0
isa		: rv64imac
mmu		: sv39
```

LoongArch example:

```text
processor	: 0
cpu family	: LoongArch
model name	: LoongArch64
```

### 5.10 `/proc/uptime`

<!-- txdoc:BRINGUP-FS-PROC-UPTIME-1 -->

Format:

```text
<uptime_seconds> <idle_seconds>
```

v1 may set idle seconds to `0.00`.

Example:

```text
12.34 0.00
```

---

## 6. RNode creation and identity

<!-- txdoc:BRINGUP-FS-RNODE-CREATION-IDENTITY-1 -->

procfs may synthesize stable `FsObjectId` values from `ProcfsKey`.

Recommended encoding:

```text
class_tag | pid | fd | local_type
```

Rules:

```text
same path/key within the same procfs mount maps to same FsObjectId while target is addressable
pid-specific entries disappear when pid becomes non-addressable
stale projected RNode reads re-observe target and return ENOENT if gone
```

This preserves procfs as projection: the RNode may survive briefly, but its read semantics are governed by current target liveness.

---

## 7. Liveness and race behavior

<!-- txdoc:BRINGUP-FS-LIVENESS-RACE-BEHAVIOR-1 -->

### 7.1 pid races

<!-- txdoc:BRINGUP-FS-PID-RACES-1 -->

If process exits while procfs lookup/read races:

```text
lookup may return ENOENT
or lookup may return a projected RNode whose read returns ENOENT later
```

Both are acceptable race-degradation outcomes.

### 7.2 fd races

<!-- txdoc:BRINGUP-FS-FD-RACES-1 -->

If fd closes while `/proc/<pid>/fd/<n>` is read:

```text
readlink may return old target if fd was observed before close
or ENOENT if close was observed first
```

No silent retarget is allowed: fd generation or binding check must prevent fd number reuse from being reported as the old fd.

### 7.3 mount races

<!-- txdoc:BRINGUP-FS-MOUNT-RACES-1 -->

If a mount is detached while `/proc/mounts` renders:

```text
render may include or omit the mount
must not render corrupted partial lines
```

---

## 8. Permissions

<!-- txdoc:BRINGUP-FS-PERMISSIONS-1 -->

Bringup v1 may use a permissive model:

```text
/proc is globally readable
/proc/<pid>/fd and /proc/<pid>/exe readable for same uid or root
```

If credentials are not yet complete:

```text
allow all reads during bringup
```

This is technical debt for the OSCOMP profile.

---

## 9. Technical debt

<!-- txdoc:BRINGUP-FS-TECHNICAL-DEBT-3 -->

```text
TD-PROCFS-1: no complete Linux /proc/<pid>/stat format
TD-PROCFS-2: no /proc/self/maps initially, unless dynamic linker profile requires it
TD-PROCFS-3: no /proc/sys
TD-PROCFS-4: no /proc/net
TD-PROCFS-5: incomplete permission model
TD-PROCFS-6: approximate meminfo/cpuinfo/uptime acceptable during bringup
TD-PROCFS-7: no hidepid or namespace-specific procfs mount options
TD-PROCFS-8: no ptrace visibility filtering
```

---

# Part IV — Integration checkpoints

<!-- txdoc:BRINGUP-FS-PART-IV-INTEGRATION-CHECKPOINTS-1 -->

## 1. Bringup required operations

<!-- txdoc:BRINGUP-FS-BRINGUP-REQUIRED-OPERATIONS-1 -->

The three specs together require the following kernel APIs:

```text
vfs::kernel_mkdir
vfs::kernel_create
vfs::kernel_write_all
vfs::kernel_symlink
vfs::kernel_chmod_chown or metadata update equivalent
page_backed::write for Anon/Persistent PCs
procfs projection read path
```

## 2. Required cross-doc edits

<!-- txdoc:BRINGUP-FS-REQUIRED-CROSS-DOC-EDITS-1 -->

```text
VFS:
    add RNodeBackingInit hook for backend-provided backing
    add kernel-internal create/write/symlink helpers for initramfs unpack
    ensure symlink/readlink path is specified

PAGE_BACKED:
    confirm tmpfs uses PageContainerKind::Anon { Persistent }

PROCESS:
    add ProcessPayload.cmdline snapshot
    add ProcessPayload.exe_file: Option<ExecutableImageRef>

MOUNT:
    ensure MountSource::MagicName works for tmpfs/procfs
    ensure root tmpfs bootstrap is documented

DEVFS/TTY:
    provide /dev/console before pid1 fd setup
```

## 3. Minimum passing scenario

<!-- txdoc:BRINGUP-FS-MINIMUM-PASSING-SCENARIO-1 -->

```text
1. BootInfo contains initrd cpio.
2. mount tmpfs as /.
3. unpack cpio into tmpfs.
4. mount devfs at /dev.
5. mount procfs at /proc.
6. create pid1.
7. open /dev/console as fd 0/1/2.
8. exec /bin/busybox sh.
9. shell runs:
       echo hello
       ls /
       cat /proc/mounts
       mkdir /tmp/a
       echo hi > /tmp/a/x
       cat /tmp/a/x
```

---

# One-paragraph summary

<!-- txdoc:BRINGUP-FS-ONE-PARAGRAPH-SUMMARY-1 -->

`TMPFS_v1` provides a writable in-memory FsOps backend whose regular files are PageBacked by `PageContainerKind::Anon { Persistent }`; `INITRAMFS_CPIO_v1` parses a trusted cpio `newc` archive from `BootInfo.initrd` and populates the tmpfs root through normal VFS operations; `PROCFS_v1` provides a minimal projected filesystem for `/proc/mounts`, `/proc/self`, `/proc/self/fd`, `/proc/self/exe`, and a few Linux-like stub files. Together they close the filesystem side of the bringup profile without taking on the full OSCOMP sdcard surface.
