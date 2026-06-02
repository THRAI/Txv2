//! VM subsystem facade arranged by subsystem anatomy.
//!
//! `structure/` owns authoritative AddressSpace recipes, RangeLock state,
//! and VM value vocabulary. `checks.rs` hosts pure observation predicates,
//! `execution.rs` hosts mutating step-like flows, and `project.rs` is the
//! read-only projection home for future procfs/sysfs adapters. Persistent
//! epoch recipe snapshots remain a staged seam; pmap materialization owns
//! HAL `PmapIf` root evidence through `pmap.rs`.

use alloc::vec::Vec;

pub mod adapter;
pub mod checks;
pub mod execution;
mod lock_metrics;
pub mod notification;
mod pmap;
pub mod project;
pub mod scripts;
pub mod step_ops;
mod structure;
mod user_access;

#[cfg(test)]
pub(crate) use pmap::TestPmap;

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub use structure::recipe_tree::bench as recipe_tree_bench;

pub use execution::{
    MadviseAdvice, MapReservation, MapReserveResult, NullUfdDispatch, UfdDispatch,
    UfdDispatchTarget,
};
pub use pmap::{PmapMappingSnapshot, PmapPublishOutcome, PmapStats, VmPmapError};
pub use scripts::{
    build_aspace_from_image, populate_detached_user_range, BssTail, ImagePlan, LoadSegment,
    ScriptError, SegmentFlags, USER_STACK_INITIAL_RESERVATION, USER_STACK_TOP_DEFAULT,
};
pub use structure::{
    AccessMode, AcquirePairResult, AcquireResult, AddressSpace, AddressSpaceStats, LockMode,
    MapPlacement, PendingWriter, PrivateFrame, PrivateFrameIdentity, PrivateFrameSnapshot,
    PrivateFrameState, PrivatePageError, PrivatePageSet, Prot, RangeGuard, RangeGuardPair,
    RangeLock, UfdRegistration, UserPage, UserPageIter, UserRange, UserRangeError, UserVirtAddr,
    VmBacking, VmEntry, VmEntryError, VmEntryFlags, VmEntryRewrite, VmFault, VmFaultError,
    VmFaultMaterialization, VmFaultMaterializationBacking, VmFaultMaterializationStep,
    VmFaultOutcome, VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget, VmPageOff,
    VmRemapOutcome, VmRemapPlacement, VmRemapRequest, WouldBlock, FULL_USER_V1_TOP,
    RANGE_LOCK_RELEASE_MASK, USER_PAGE_SIZE,
};
pub use user_access::UserAccessKind;

pub fn reset_debug_phase_totals() {
    structure::reset_private_page_debug_totals();
    structure::reset_recipe_debug_totals();
    pmap::reset_pmap_debug_totals();
}

pub fn dump_debug_phase_totals<P: tx_hal::ConsoleIf>() {
    let private = structure::private_page_debug_totals();
    let private_avg_ns = if private.count == 0 {
        0
    } else {
        private.total_ns / private.count
    };
    write_debug_line::<P>(
        ":vm:phase-total:private_set.install",
        &[
            ("count", private.count),
            ("total_ns", private.total_ns),
            ("avg_ns", private_avg_ns),
            ("max_ns", private.max_ns),
            ("treap_touched_total", private.touched_total),
            ("treap_touched_max", private.touched_max),
            ("treap_node_alloc_total", private.node_alloc_total),
            ("treap_node_alloc_max", private.node_alloc_max),
            ("len_max", private.len_max),
            ("sample_count", private.sample_count),
            ("sample_dropped", private.sample_dropped),
        ],
    );
    dump_private_install_distribution::<P>();

    let recipe = structure::recipe_debug_totals();
    let recipe_avg_ns = if recipe.op_count == 0 {
        0
    } else {
        recipe.op_total_ns / recipe.op_count
    };
    write_debug_line::<P>(
        ":vm:phase-total:recipe.publish",
        &[
            ("count", recipe.op_count),
            ("total_ns", recipe.op_total_ns),
            ("avg_ns", recipe_avg_ns),
            ("max_ns", recipe.op_max_ns),
            ("touched_total", recipe.op_touched_total),
            ("node_alloc_total", recipe.op_node_alloc_total),
            ("node_alloc_max", recipe.op_node_alloc_max),
            ("node_alloc_count", recipe.node_alloc_count),
            ("chunk_alloc_count", recipe.chunk_alloc_count),
        ],
    );
    let recipe_reclaim_avg_ns = if recipe.reclaim_count == 0 {
        0
    } else {
        recipe.reclaim_total_ns / recipe.reclaim_count
    };
    write_debug_line::<P>(
        ":vm:phase-total:recipe.reclaim_tree",
        &[
            ("count", recipe.reclaim_count),
            ("total_ns", recipe.reclaim_total_ns),
            ("avg_ns", recipe_reclaim_avg_ns),
            ("max_ns", recipe.reclaim_max_ns),
            ("node_total", recipe.reclaim_node_total),
            ("node_max", recipe.reclaim_node_max),
        ],
    );

    let pmap = pmap::pmap_debug_totals();
    let pmap_avg_ns = if pmap.batch_insert_count == 0 {
        0
    } else {
        pmap.batch_insert_total_ns / pmap.batch_insert_count
    };
    write_debug_line::<P>(
        ":vm:phase-total:pmap.publish_batch.insert",
        &[
            ("count", pmap.batch_insert_count),
            ("total_ns", pmap.batch_insert_total_ns),
            ("avg_ns", pmap_avg_ns),
            ("max_ns", pmap.batch_insert_max_ns),
        ],
    );
    let pmap_remove_avg_ns = if pmap.teardown_remove_count == 0 {
        0
    } else {
        pmap.teardown_remove_total_ns / pmap.teardown_remove_count
    };
    write_debug_line::<P>(
        ":vm:phase-total:pmap.teardown.remove",
        &[
            ("count", pmap.teardown_remove_count),
            ("total_ns", pmap.teardown_remove_total_ns),
            ("avg_ns", pmap_remove_avg_ns),
            ("max_ns", pmap.teardown_remove_max_ns),
            ("shifted_total", pmap.teardown_remove_shifted_total),
            ("shifted_max", pmap.teardown_remove_shifted_max),
        ],
    );
    write_debug_line::<P>(
        ":vm:phase-total:private_anon.prefault",
        &[
            ("private_installs", private.count),
            ("batch_pmap_publishes", pmap.batch_insert_count),
            (
                "non_batch_private_installs",
                private.count.saturating_sub(pmap.batch_insert_count),
            ),
        ],
    );
}

fn dump_private_install_distribution<P: tx_hal::ConsoleIf>() {
    let samples = structure::private_page_debug_samples();
    if samples.is_empty() {
        return;
    }

    let mut durations: Vec<u64> = samples.iter().map(|sample| sample.duration_ns).collect();
    durations.sort_unstable();
    write_debug_line::<P>(
        ":vm:phase-percentile:private_set.install",
        &[
            ("sample_count", durations.len() as u64),
            ("p50_ns", percentile_sorted(&durations, 50)),
            ("p95_ns", percentile_sorted(&durations, 95)),
            ("p99_ns", percentile_sorted(&durations, 99)),
            ("max_ns", durations[durations.len() - 1]),
        ],
    );

    for (bucket_id, (len_min, len_max)) in PRIVATE_INSTALL_LEN_BUCKETS.iter().enumerate() {
        let mut bucket: Vec<u64> = samples
            .iter()
            .filter(|sample| sample.len_before >= *len_min && sample.len_before <= *len_max)
            .map(|sample| sample.duration_ns)
            .collect();
        if bucket.is_empty() {
            continue;
        }
        bucket.sort_unstable();
        let total_ns = bucket.iter().copied().sum::<u64>();
        write_debug_line::<P>(
            ":vm:phase-bucket:private_set.install",
            &[
                ("bucket", bucket_id as u64),
                ("len_min", *len_min),
                ("len_max", *len_max),
                ("count", bucket.len() as u64),
                ("total_ns", total_ns),
                ("avg_ns", total_ns / bucket.len() as u64),
                ("p50_ns", percentile_sorted(&bucket, 50)),
                ("p95_ns", percentile_sorted(&bucket, 95)),
                ("max_ns", bucket[bucket.len() - 1]),
            ],
        );
    }
}

const PRIVATE_INSTALL_LEN_BUCKETS: &[(u64, u64)] = &[
    (0, 16),
    (17, 64),
    (65, 256),
    (257, 1024),
    (1025, 4096),
    (4097, u64::MAX),
];

fn percentile_sorted(values: &[u64], percentile: u64) -> u64 {
    debug_assert!(!values.is_empty());
    let rank = ((values.len() as u64 * percentile).saturating_add(99) / 100).saturating_sub(1);
    values[rank.min((values.len() - 1) as u64) as usize]
}

fn write_debug_line<P: tx_hal::ConsoleIf>(label: &str, fields: &[(&str, u64)]) {
    tx_hal::console_write_str::<P>(label);
    for (name, value) in fields {
        tx_hal::console_write_str::<P>(" ");
        tx_hal::console_write_str::<P>(name);
        tx_hal::console_write_str::<P>("=");
        write_u64_decimal::<P>(*value);
    }
    tx_hal::console_write_str::<P>("\n");
}

fn write_u64_decimal<P: tx_hal::ConsoleIf>(mut value: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if value == 0 {
        tx_hal::console_write_str::<P>("0");
        return;
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    // SAFETY: only ASCII decimal digits are written into `buf[i..]`.
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    tx_hal::console_write_str::<P>(s);
}
