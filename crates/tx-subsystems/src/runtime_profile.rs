//! Lightweight runtime counters for LTP command-window profiling.
//!
//! This module deliberately records counters only.  The expensive part of the
//! earlier `trap-trace` path was printing every trap; these atomics let the LTP
//! command wrappers sample cumulative totals at begin/end markers instead.

use core::sync::atomic::{AtomicU64, Ordering};

static SYSCALLS: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);
static IPF: AtomicU64 = AtomicU64::new(0);
static LPF: AtomicU64 = AtomicU64::new(0);
static SPF: AtomicU64 = AtomicU64::new(0);
static UPF: AtomicU64 = AtomicU64::new(0);

static READ: AtomicU64 = AtomicU64::new(0);
static READ_LE1: AtomicU64 = AtomicU64::new(0);
static READ_REQUESTED: AtomicU64 = AtomicU64::new(0);
static WRITE: AtomicU64 = AtomicU64::new(0);
static WRITE_REQUESTED: AtomicU64 = AtomicU64::new(0);
static PPOLL: AtomicU64 = AtomicU64::new(0);
static PSELECT6: AtomicU64 = AtomicU64::new(0);
static WAIT4: AtomicU64 = AtomicU64::new(0);
static CLONE: AtomicU64 = AtomicU64::new(0);
static EXECVE: AtomicU64 = AtomicU64::new(0);
static OPENAT: AtomicU64 = AtomicU64::new(0);
static CLOSE: AtomicU64 = AtomicU64::new(0);
static DUP3: AtomicU64 = AtomicU64::new(0);
static FCNTL: AtomicU64 = AtomicU64::new(0);
static PIPE2: AtomicU64 = AtomicU64::new(0);
static NEWFSTATAT: AtomicU64 = AtomicU64::new(0);
static BRK: AtomicU64 = AtomicU64::new(0);
static MMAP: AtomicU64 = AtomicU64::new(0);
static MPROTECT: AtomicU64 = AtomicU64::new(0);
static MUNMAP: AtomicU64 = AtomicU64::new(0);

// VM recipe-tree republish accounting. Every `RecipeIndex` mutator (map,
// unmap, protect, fork's batched commit, exec's per-segment registration)
// swaps in a freshly cloned tree through `RecipeIndex::publish`. These two
// counters expose how often that happens and how many entries the published
// trees carry, so a trace can see exec's O(M^2) per-segment republish versus
// fork's single O(M) batched commit — without timing instrumentation.
static RECIPE_PUBLISHES: AtomicU64 = AtomicU64::new(0);
static RECIPE_ENTRY_COPIES: AtomicU64 = AtomicU64::new(0);
// Peak published-tree size seen across all aspaces. A heap that fragments
// (one anon VmEntry per page-crossing brk growth) drives this up, so it
// quantifies the worst-case whole-tree-clone cost and bounds the win from
// coalescing brk growth into the adjacent heap entry.
static RECIPE_MAX_TREE: AtomicU64 = AtomicU64::new(0);
// Per-source recipe-publish attribution: which RecipeIndex mutator triggered
// each whole-tree-clone publish. Sum should equal RECIPE_PUBLISHES; pins down
// where the publish volume actually comes from.
static PUB_MAP: AtomicU64 = AtomicU64::new(0);
static PUB_MAP_MANY: AtomicU64 = AtomicU64::new(0);
static PUB_UNMAP: AtomicU64 = AtomicU64::new(0);
static PUB_PROTECT: AtomicU64 = AtomicU64::new(0);
static PUB_MLOCK: AtomicU64 = AtomicU64::new(0);
static PUB_REMAP: AtomicU64 = AtomicU64::new(0);
static PUB_UFD: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub syscalls: u64,
    pub faults: u64,
    pub ipf: u64,
    pub lpf: u64,
    pub spf: u64,
    pub upf: u64,
    pub read: u64,
    pub read_le1: u64,
    pub read_requested: u64,
    pub write: u64,
    pub write_requested: u64,
    pub ppoll: u64,
    pub pselect6: u64,
    pub wait4: u64,
    pub clone: u64,
    pub execve: u64,
    pub openat: u64,
    pub close: u64,
    pub dup3: u64,
    pub fcntl: u64,
    pub pipe2: u64,
    pub newfstatat: u64,
    pub brk: u64,
    pub mmap: u64,
    pub mprotect: u64,
    pub munmap: u64,
    pub recipe_publishes: u64,
    pub recipe_entry_copies: u64,
    pub recipe_max_tree: u64,
    pub pub_map: u64,
    pub pub_map_many: u64,
    pub pub_unmap: u64,
    pub pub_protect: u64,
    pub pub_mlock: u64,
    pub pub_remap: u64,
    pub pub_ufd: u64,
}

pub fn record_syscall(nr: u64, args: [u64; 6]) {
    SYSCALLS.fetch_add(1, Ordering::Relaxed);
    match nr {
        24 => {
            DUP3.fetch_add(1, Ordering::Relaxed);
        }
        25 => {
            FCNTL.fetch_add(1, Ordering::Relaxed);
        }
        56 => {
            OPENAT.fetch_add(1, Ordering::Relaxed);
        }
        57 => {
            CLOSE.fetch_add(1, Ordering::Relaxed);
        }
        59 => {
            PIPE2.fetch_add(1, Ordering::Relaxed);
        }
        63 => {
            READ.fetch_add(1, Ordering::Relaxed);
            READ_REQUESTED.fetch_add(args[2], Ordering::Relaxed);
            if args[2] <= 1 {
                READ_LE1.fetch_add(1, Ordering::Relaxed);
            }
        }
        64 => {
            WRITE.fetch_add(1, Ordering::Relaxed);
            WRITE_REQUESTED.fetch_add(args[2], Ordering::Relaxed);
        }
        72 => {
            PSELECT6.fetch_add(1, Ordering::Relaxed);
        }
        73 => {
            PPOLL.fetch_add(1, Ordering::Relaxed);
        }
        79 => {
            NEWFSTATAT.fetch_add(1, Ordering::Relaxed);
        }
        214 => {
            BRK.fetch_add(1, Ordering::Relaxed);
        }
        215 => {
            MUNMAP.fetch_add(1, Ordering::Relaxed);
        }
        220 => {
            CLONE.fetch_add(1, Ordering::Relaxed);
        }
        221 => {
            EXECVE.fetch_add(1, Ordering::Relaxed);
        }
        222 => {
            MMAP.fetch_add(1, Ordering::Relaxed);
        }
        226 => {
            MPROTECT.fetch_add(1, Ordering::Relaxed);
        }
        260 => {
            WAIT4.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

pub fn record_user_fault(write: bool, instruction: bool) {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    if instruction {
        IPF.fetch_add(1, Ordering::Relaxed);
    } else if write {
        SPF.fetch_add(1, Ordering::Relaxed);
    } else {
        LPF.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_unknown_user_fault() {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    UPF.fetch_add(1, Ordering::Relaxed);
}

/// Record one recipe-tree publication and the number of entries in the newly
/// published tree (a proxy for the per-publish whole-tree clone cost). Called
/// from `RecipeIndex::publish`; cheap relaxed atomics, safe in any context.
pub fn record_recipe_publish(published_entries: u64) {
    RECIPE_PUBLISHES.fetch_add(1, Ordering::Relaxed);
    RECIPE_ENTRY_COPIES.fetch_add(published_entries, Ordering::Relaxed);
    RECIPE_MAX_TREE.fetch_max(published_entries, Ordering::Relaxed);
}

/// Source of a recipe-tree publish, for per-caller attribution.
#[derive(Clone, Copy)]
pub enum RecipeSource {
    Map,
    MapMany,
    Unmap,
    Protect,
    Mlock,
    Remap,
    Ufd,
}

/// Record which `RecipeIndex` mutator triggered a recipe publish.
pub fn record_recipe_source(src: RecipeSource) {
    let counter = match src {
        RecipeSource::Map => &PUB_MAP,
        RecipeSource::MapMany => &PUB_MAP_MANY,
        RecipeSource::Unmap => &PUB_UNMAP,
        RecipeSource::Protect => &PUB_PROTECT,
        RecipeSource::Mlock => &PUB_MLOCK,
        RecipeSource::Remap => &PUB_REMAP,
        RecipeSource::Ufd => &PUB_UFD,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        syscalls: SYSCALLS.load(Ordering::Relaxed),
        faults: FAULTS.load(Ordering::Relaxed),
        ipf: IPF.load(Ordering::Relaxed),
        lpf: LPF.load(Ordering::Relaxed),
        spf: SPF.load(Ordering::Relaxed),
        upf: UPF.load(Ordering::Relaxed),
        read: READ.load(Ordering::Relaxed),
        read_le1: READ_LE1.load(Ordering::Relaxed),
        read_requested: READ_REQUESTED.load(Ordering::Relaxed),
        write: WRITE.load(Ordering::Relaxed),
        write_requested: WRITE_REQUESTED.load(Ordering::Relaxed),
        ppoll: PPOLL.load(Ordering::Relaxed),
        pselect6: PSELECT6.load(Ordering::Relaxed),
        wait4: WAIT4.load(Ordering::Relaxed),
        clone: CLONE.load(Ordering::Relaxed),
        execve: EXECVE.load(Ordering::Relaxed),
        openat: OPENAT.load(Ordering::Relaxed),
        close: CLOSE.load(Ordering::Relaxed),
        dup3: DUP3.load(Ordering::Relaxed),
        fcntl: FCNTL.load(Ordering::Relaxed),
        pipe2: PIPE2.load(Ordering::Relaxed),
        newfstatat: NEWFSTATAT.load(Ordering::Relaxed),
        brk: BRK.load(Ordering::Relaxed),
        mmap: MMAP.load(Ordering::Relaxed),
        mprotect: MPROTECT.load(Ordering::Relaxed),
        munmap: MUNMAP.load(Ordering::Relaxed),
        recipe_publishes: RECIPE_PUBLISHES.load(Ordering::Relaxed),
        recipe_entry_copies: RECIPE_ENTRY_COPIES.load(Ordering::Relaxed),
        recipe_max_tree: RECIPE_MAX_TREE.load(Ordering::Relaxed),
        pub_map: PUB_MAP.load(Ordering::Relaxed),
        pub_map_many: PUB_MAP_MANY.load(Ordering::Relaxed),
        pub_unmap: PUB_UNMAP.load(Ordering::Relaxed),
        pub_protect: PUB_PROTECT.load(Ordering::Relaxed),
        pub_mlock: PUB_MLOCK.load(Ordering::Relaxed),
        pub_remap: PUB_REMAP.load(Ordering::Relaxed),
        pub_ufd: PUB_UFD.load(Ordering::Relaxed),
    }
}
