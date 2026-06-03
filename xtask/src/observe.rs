//! `cargo xtask observe` — observation pipeline wrappers.
//!
//! Subcommands:
//! - `replay` — decode a `.txtrace` file and emit NDJSON (default) or Perfetto.
//! - `pftrace` — convenience alias: replay with `--out pftrace --output <path>`.
//! - `validate` — parse header + walk slots, print a one-line summary.
//! - `demo` — generate a small synthetic `.txtrace` file for testing.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::{Object, ObjectSymbol};

use crate::util::{command_exists, optional_option_value, run_cmd_owned};
use crate::Result;

/// Packed demo-record descriptor: (kind, level, name, span, parent, payload_tag, payload_len, payload).
type DemoRecord = (u8, u8, u32, u64, u64, u16, u16, [u8; 16]);

pub(crate) fn observe(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(subcmd) = args.first() else {
        return Err(
            "observe command needs replay, pftrace, validate, demo, extract, bundle, or live-guest-mem\n\
             usage:\n\
             \tcargo xtask observe replay --file <path> [--out json|pftrace] [--output <out>] [--filter level=N]\n\
            \tcargo xtask observe analyze (--file <txtrace>|--rawrecords <trace.rawrecords>|--ndjson <replay.ndjson>) [--names <names.json>] [--top N] [--roundtrip-sysno N] [--cache-dir <dir>] [--parquet-dir <dir>] [--sql <query>|--sql-file <query.sql>|--python-file <script.py>]\n\
             \tcargo xtask observe pftrace --file <path> --output <pftrace>\n\
             \tcargo xtask observe validate --file <path>\n\
             \tcargo xtask observe demo --output <path> [--records N] [--with-yields]\n\
             \tcargo xtask observe extract --serial <log> --output <txtrace>\n\
             \tcargo xtask observe bundle (--file <txtrace>|--serial <log>) --output-dir <dir> [--names <names.json>] [--kernel <elf>]\n\
             \tcargo xtask observe live-guest-mem --guest-mem <ram-file> --kernel <elf> --output-dir <dir> [--stop-file <path>] [--finalize]\n\
             \tcargo xtask observe oscomp-live [--test pthread|vm|stdio|regex|...] [--output-dir <dir>] [--python-file <script.py>]\n\
             \tcargo xtask observe names --kernel <elf> [--output <names.json>]"
                .into(),
        );
    };
    match subcmd.as_str() {
        "replay" => observe_replay(root, &args[1..]),
        "analyze" => observe_analyze(root, &args[1..]),
        "pftrace" => observe_pftrace(root, &args[1..]),
        "validate" => observe_validate(&args[1..]),
        "demo" => observe_demo(&args[1..]),
        "extract" => observe_extract(&args[1..]),
        "bundle" => observe_bundle(root, &args[1..]),
        "live-guest-mem" => observe_live_guest_mem(root, &args[1..]),
        "oscomp-live" => observe_oscomp_live(root, &args[1..]),
        "names" => observe_names(&args[1..]),
        other => Err(format!(
            "unknown observe subcommand '{other}'; \
             expected replay, analyze, pftrace, validate, demo, extract, bundle, live-guest-mem, oscomp-live, or names"
        )),
    }
}

// ── names ─────────────────────────────────────────────────────────────────────

/// Build a `names.json` from a kernel ELF.
///
/// The kernel's `tx_observe::fnv1a32` and `tx_scripts::drive::op_name_id<S>`
/// compute every observation `EventNameId` as `fnv1a32(literal_bytes)` —
/// either a fixed string (`b"resume"`, `b"wake.notify"`, …) or the result
/// of `core::any::type_name::<S>().as_bytes()`. Both forms produce string
/// literals that `rustc` embeds in the binary's read-only data — the
/// type-name strings live in `.rodata.cst*` / `.rodata` as `\0`-free byte
/// runs prefixed/suffixed by binary padding.
///
/// This scanner reads the kernel ELF, walks every read-only section, and
/// extracts every plausibly-Rust-type-name byte run (containing `::` and
/// composed of identifier-safe + a small punctuation alphabet). Each
/// extracted string is FNV-1a 32 hashed and added to a `name_table`. A
/// static prelude carries the well-known mappings (Linux RV64 syscall
/// numbers, kernel-emitted stable names like `resume` / `step` /
/// `yield.*` / `wake.notify`, mutation ASCII tags) that aren't
/// recoverable from the ELF alone.
///
/// Usage: `cargo xtask observe names --kernel <elf> [--output <json>]`.
/// With `--output` omitted, prints the JSON on stdout.
fn observe_names(args: &[String]) -> Result<()> {
    let kernel = optional_option_value(args, "--kernel")
        .map(PathBuf::from)
        .ok_or("--kernel <elf> is required for names subcommand")?;
    let bytes = fs::read(&kernel)
        .map_err(|e| format!("cannot read kernel ELF {}: {e}", kernel.display()))?;
    let file = object::File::parse(bytes.as_slice())
        .map_err(|e| format!("ELF parse failed for {}: {e}", kernel.display()))?;

    let mut table = std::collections::BTreeMap::<u32, String>::new();

    // ── 1. Static prelude ───────────────────────────────────────────────
    for (nr, name) in linux_rv64_syscalls().iter().copied() {
        table.entry(u32::from(nr)).or_insert(format!("sys_{name}"));
    }
    for &(s, label) in KERNEL_FNV1A_STABLE_NAMES.iter() {
        table
            .entry(fnv1a32(s.as_bytes()))
            .or_insert(label.to_string());
    }
    // Mutation tag literals — packed `u32` of the ASCII bytes, emitted in
    // `tx_substrate::zone` / `index` and NOT routed through `fnv1a32`.
    table
        .entry(0x4d5a5347)
        .or_insert("mutation.zone_sign".into());
    table
        .entry(0x4d494358)
        .or_insert("mutation.index_commit".into());

    // OBS-9 Sched span labels — namespaced under `SCHED_NAME_BASE`
    // (0x8000_0000) by `tx_reactor::runtime::emit_sched_begin`. Pre-
    // populate `task.<N>` for the first 1024 TIDs so Perfetto renders
    // sched slices as `task.<tid>` instead of falling back to
    // `name_0x8…`.
    const SCHED_NAME_BASE: u32 = 0x8000_0000;
    for tid in 0..1024u32 {
        table
            .entry(SCHED_NAME_BASE | tid)
            .or_insert(format!("task.{tid}"));
    }

    // Syscall `ArgValue` continuation keys — `tx_shims::linux_syscall::dispatch`
    // emits one `Instant(ArgValue)` per register-shaped syscall arg
    // (`a0`..`a5`) right after `SpanBegin(SyscallEnter)`. The daemon
    // surfaces these as debug annotations on the `sys_*` slice; the
    // name lookup goes through this same `names.json` table.
    for name in ["a0", "a1", "a2", "a3", "a4", "a5"] {
        table
            .entry(fnv1a32(name.as_bytes()))
            .or_insert(name.to_string());
    }

    // ── 2. Symbol-table walk for type-name-based observe ids ────────────
    //
    // Per OBS-V1-OPNAME-1 and OBS-HOST-V0-NAMES-GENERATION: the
    // names.json file is produced by an xtask in the kernel build that
    // walks the ELF's symbol table.
    //
    // rustc emits one `tx_scripts::drive::op_name_id` symbol per
    // concrete `S` reached by `drive::<S, _>`. Demangling each symbol
    // reveals `tx_scripts::drive::op_name_id::<S>` where `S` is the
    // exact `type_name::<S>()` the kernel will hash at the call site.
    // We extract `S`, hash with `fnv1a32` (matching `tx_observe::fnv1a32`
    // bit-for-bit), and register `drive.<LastSegment>` as the readable
    // label — short names keep Perfetto chip widths usable; the full
    // path is recoverable from the symbol table if needed.
    // For each `op_name_id::<S>` symbol we register two `EventNameId`
    // candidates because rustc symbol mangling erases lifetimes while
    // `core::any::type_name::<S>()` keeps them as `<'_>`:
    //
    //   symbol           : tx_subsystems::vfs::execution::OpenFileReadOp
    //   type_name output : tx_subsystems::vfs::execution::OpenFileReadOp<'_>
    //
    // Both hash to different `u32`s; the trace carries whichever the kernel
    // emitted at the call site (bare for lifetime-free types like
    // `OpenOp`, `<'_>` for lifetime-parameterised types like
    // `OpenFileReadOp<'a>`). Registering both keeps the daemon resolution
    // independent of whether the source type has a lifetime parameter,
    // which is not recoverable from the v0 symbol alone.
    let mut drives_found = 0usize;
    for sym in file.symbols() {
        let Ok(raw) = sym.name() else { continue };
        let demangled = demangle_symbol(raw);
        if let Some(s) = extract_op_name_id_type_param(&demangled) {
            let short = short_type(s);
            let label = format!("drive.{short}");
            table.entry(fnv1a32(s.as_bytes())).or_insert(label.clone());
            let with_lifetime = format!("{s}<'_>");
            table
                .entry(fnv1a32(with_lifetime.as_bytes()))
                .or_insert(label);
            drives_found += 1;
        }
        if let Some(s) = extract_ds_zone_type_param(&demangled) {
            table
                .entry(fnv1a32(s.as_bytes()))
                .or_insert(format!("zone.{}", short_type(s)));
        }
    }

    // ── 3. Render JSON ──────────────────────────────────────────────────
    let mut json = String::from("{\n  \"name_table\": {\n");
    let n = table.len();
    for (i, (k, v)) in table.iter().enumerate() {
        let sep = if i + 1 == n { "" } else { "," };
        json.push_str(&format!("    \"{k}\": \"{v}\"{sep}\n"));
    }
    json.push_str("  }\n}\n");

    match optional_option_value(args, "--output") {
        Some(out) => {
            let p = PathBuf::from(&out);
            fs::write(&p, json.as_bytes())
                .map_err(|e| format!("failed to write {}: {e}", p.display()))?;
            eprintln!(
                "observe: names.json written to {} ({} entries, \
                 {} `op_name_id::<S>` monomorphizations resolved from symbol table)",
                p.display(),
                table.len(),
                drives_found,
            );
        }
        None => {
            print!("{json}");
        }
    }
    Ok(())
}

/// Demangle to the alternate (`{:#}`) form, which strips the `[hashhex]`
/// crate disambiguator that v0 symbol mangling carries on every
/// crate-qualified path. Without `{:#}` the output reads as
/// `tx_scripts[b63d…]::drive::op_name_id::<tx_subsystems[c8d9…]::…::X>`
/// and the substring pivot in `extract_op_name_id_type_param` fails to
/// match. With `{:#}` it reads as plain `tx_scripts::drive::op_name_id::<…>`,
/// which is the exact byte sequence the kernel-side `type_name::<X>()`
/// produces and FNV-1a hashes.
/// Locate a kernel ELF for `observe names` to scan, in this order:
///
/// 1. Explicit `--kernel <elf>` argument on the caller's command line.
/// 2. `target/oscomp/submit/kernel-rv` — what `cargo xtask oscomp submit`
///    drops; the natural shape for an oscomp trace pipeline.
/// 3. `target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt`
///    — the direct cargo build output, used when running smoke / qemu
///    profiles without going through `oscomp`.
///
/// Returns `None` when none of these exist; the caller proceeds without
/// `--names` and the daemon falls back to `name_0xNN` hex labels.
fn resolve_names_kernel_elf(root: &Path, args: &[String]) -> Option<PathBuf> {
    if let Some(k) = optional_option_value(args, "--kernel") {
        let p = PathBuf::from(k);
        if p.exists() {
            return Some(p);
        }
    }
    for rel in [
        "target/oscomp/submit/kernel-rv",
        "target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt",
    ] {
        let p = root.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn demangle_symbol(name: &str) -> String {
    match rustc_demangle::try_demangle(name) {
        Ok(d) => format!("{d:#}"),
        Err(_) => name.to_string(),
    }
}

/// Pull `S` out of a demangled `…::op_name_id::<S>` symbol, taking
/// `<>` nesting into account so generic-with-generic types like
/// `Cap<ProcessIdentity>` survive intact. Returns `None` when the
/// symbol is anything else (most symbols).
///
/// rustc emits `op_name_id` as either:
///   * `tx_scripts::drive::op_name_id::<X>`
///   * `tx_scripts::drive::op_name_id::<X>::ha34b1d6e23bf99c0`
///     (legacy mangling appends a per-crate-version hash trailer)
///
/// Both forms are handled — we read up to the first balanced `>` and
/// stop, leaving any trailer untouched.
fn extract_op_name_id_type_param(demangled: &str) -> Option<&str> {
    const PIVOT: &str = "tx_scripts::drive::op_name_id::<";
    extract_first_type_param_after(demangled, PIVOT)
}

fn extract_ds_zone_type_param(demangled: &str) -> Option<&str> {
    for pivot in [
        "tx_substrate::zone::reserve_for::<",
        "tx_substrate::zone::sign_for::<",
        "tx_substrate::zone::sign::<",
        "tx_substrate::zone::return_slot::<",
        "tx_substrate::zone::return_slot_from_reclaim::<",
    ] {
        if let Some(type_param) = extract_first_type_param_after(demangled, pivot) {
            return Some(type_param);
        }
    }
    None
}

fn extract_first_type_param_after<'a>(demangled: &'a str, pivot: &str) -> Option<&'a str> {
    let start = demangled.find(pivot)? + pivot.len();
    let rest = &demangled[start..];
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// FNV-1a 32 — matches `tx_observe::fnv1a32` exactly.
fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Last segment after `::` (e.g. `OpenFileReadOp<'_>` → `OpenFileReadOp`).
fn short_type(full: &str) -> String {
    let last = full.rsplit("::").next().unwrap_or(full);
    last.trim_end_matches("<'_>").to_string()
}

const KERNEL_FNV1A_STABLE_NAMES: &[(&str, &str)] = &[
    ("debug.alloc.zone.slab", "debug.alloc.zone.slab"),
    ("debug.alloc.zone.slab.id", "debug.alloc.zone.slab.id"),
    ("debug.alloc.zone.slab.slots", "debug.alloc.zone.slab.slots"),
    ("debug.alloc.page_frame", "debug.alloc.page_frame"),
    ("debug.alloc.page_frame.ppn", "debug.alloc.page_frame.ppn"),
    ("debug.alloc.page_run", "debug.alloc.page_run"),
    ("debug.alloc.page_run.base", "debug.alloc.page_run.base"),
    ("debug.alloc.page_run.count", "debug.alloc.page_run.count"),
    ("debug.alloc.vm.recipe_node", "debug.alloc.vm.recipe_node"),
    (
        "debug.alloc.vm.recipe_node.subtree_len",
        "debug.alloc.vm.recipe_node.subtree_len",
    ),
    (
        "debug.alloc.vm.private_page_node",
        "debug.alloc.vm.private_page_node",
    ),
    (
        "debug.alloc.vm.private_page_node.subtree_len",
        "debug.alloc.vm.private_page_node.subtree_len",
    ),
    (
        "debug.alloc.pagebacked.cache",
        "debug.alloc.pagebacked.cache",
    ),
    (
        "debug.alloc.pagebacked.cache.page",
        "debug.alloc.pagebacked.cache.page",
    ),
    (
        "debug.alloc.pagebacked.cache.len",
        "debug.alloc.pagebacked.cache.len",
    ),
    (
        "debug.alloc.vm.address_space",
        "debug.alloc.vm.address_space",
    ),
    (
        "debug.alloc.vm.address_space.cap",
        "debug.alloc.vm.address_space.cap",
    ),
    (
        "debug.alloc.pagebacked.container",
        "debug.alloc.pagebacked.container",
    ),
    (
        "debug.alloc.pagebacked.container.pages",
        "debug.alloc.pagebacked.container.pages",
    ),
    ("debug.ds.method.duration_ns", "debug.ds.method.duration_ns"),
    ("debug.ds.method.zone_id", "debug.ds.method.zone_id"),
    (
        "debug.ds.substrate.zone.reserve_for",
        "debug.ds.substrate.zone.reserve_for",
    ),
    (
        "debug.ds.substrate.zone.sign_for",
        "debug.ds.substrate.zone.sign_for",
    ),
    (
        "debug.ds.substrate.zone.sign",
        "debug.ds.substrate.zone.sign",
    ),
    (
        "debug.ds.substrate.zone.pop_free_slot",
        "debug.ds.substrate.zone.pop_free_slot",
    ),
    (
        "debug.ds.substrate.zone.return_slot",
        "debug.ds.substrate.zone.return_slot",
    ),
    (
        "debug.ds.substrate.zone.return_slot_from_reclaim",
        "debug.ds.substrate.zone.return_slot_from_reclaim",
    ),
    (
        "debug.ds.substrate.zone.refill_bucket",
        "debug.ds.substrate.zone.refill_bucket",
    ),
    (
        "debug.ds.substrate.zone.drain_bucket_to_keg",
        "debug.ds.substrate.zone.drain_bucket_to_keg",
    ),
    (
        "debug.ds.substrate.page_allocator.reserve_frame",
        "debug.ds.substrate.page_allocator.reserve_frame",
    ),
    (
        "debug.ds.substrate.page_allocator.reserve_run",
        "debug.ds.substrate.page_allocator.reserve_run",
    ),
    (
        "debug.ds.process.pid_namespace.register_pid",
        "debug.ds.process.pid_namespace.register_pid",
    ),
    (
        "debug.ds.process.pid_namespace.register_tid",
        "debug.ds.process.pid_namespace.register_tid",
    ),
    (
        "debug.ds.process.pid_namespace.register_pgrp",
        "debug.ds.process.pid_namespace.register_pgrp",
    ),
    (
        "debug.ds.process.pid_namespace.register_session",
        "debug.ds.process.pid_namespace.register_session",
    ),
    (
        "debug.ds.process.pid_namespace.unregister_pid_number",
        "debug.ds.process.pid_namespace.unregister_pid_number",
    ),
    (
        "debug.ds.process.pid_namespace.unregister_tid_number",
        "debug.ds.process.pid_namespace.unregister_tid_number",
    ),
    (
        "debug.ds.process.pid_namespace.resolve_pid_number",
        "debug.ds.process.pid_namespace.resolve_pid_number",
    ),
    (
        "debug.ds.process.pid_namespace.resolve_pid_number_as",
        "debug.ds.process.pid_namespace.resolve_pid_number_as",
    ),
    (
        "debug.ds.process.pid_namespace.with_namespace",
        "debug.ds.process.pid_namespace.with_namespace",
    ),
    (
        "debug.ds.process.children.attach",
        "debug.ds.process.children.attach",
    ),
    (
        "debug.ds.process.children.detach",
        "debug.ds.process.children.detach",
    ),
    (
        "debug.ds.process.children.len",
        "debug.ds.process.children.len",
    ),
    (
        "debug.ds.process.children.is_empty",
        "debug.ds.process.children.is_empty",
    ),
    (
        "debug.ds.process.children.snapshot",
        "debug.ds.process.children.snapshot",
    ),
    (
        "debug.ds.process.children.drain",
        "debug.ds.process.children.drain",
    ),
    (
        "debug.ds.process.children.retain",
        "debug.ds.process.children.retain",
    ),
    (
        "debug.ds.process.group_members.attach",
        "debug.ds.process.group_members.attach",
    ),
    (
        "debug.ds.process.group_members.detach",
        "debug.ds.process.group_members.detach",
    ),
    (
        "debug.ds.process.group_members.len",
        "debug.ds.process.group_members.len",
    ),
    (
        "debug.ds.process.group_members.is_empty",
        "debug.ds.process.group_members.is_empty",
    ),
    (
        "debug.ds.process.group_members.retain",
        "debug.ds.process.group_members.retain",
    ),
    (
        "debug.ds.process.group_members.snapshot_live",
        "debug.ds.process.group_members.snapshot_live",
    ),
    (
        "debug.ds.process.group_members.count_live",
        "debug.ds.process.group_members.count_live",
    ),
    (
        "debug.ds.process.threads.attach",
        "debug.ds.process.threads.attach",
    ),
    (
        "debug.ds.process.threads.detach",
        "debug.ds.process.threads.detach",
    ),
    (
        "debug.ds.process.threads.count",
        "debug.ds.process.threads.count",
    ),
    (
        "debug.ds.process.threads.nth",
        "debug.ds.process.threads.nth",
    ),
    (
        "debug.ds.process.threads.find_by_tid",
        "debug.ds.process.threads.find_by_tid",
    ),
    (
        "debug.ds.process.threads.snapshot",
        "debug.ds.process.threads.snapshot",
    ),
    (
        "debug.ds.process.threads.drain",
        "debug.ds.process.threads.drain",
    ),
    (
        "debug.ds.process.threads.retain",
        "debug.ds.process.threads.retain",
    ),
    (
        "debug.ds.process.session_members.attach",
        "debug.ds.process.session_members.attach",
    ),
    (
        "debug.ds.process.session_members.len",
        "debug.ds.process.session_members.len",
    ),
    (
        "debug.ds.process.session_members.is_empty",
        "debug.ds.process.session_members.is_empty",
    ),
    (
        "debug.ds.process.session_members.snapshot_live",
        "debug.ds.process.session_members.snapshot_live",
    ),
    (
        "debug.signal.select.thread1.lock.request",
        "debug.signal.select.thread1.lock.request",
    ),
    (
        "debug.signal.select.thread1.lock.acquired",
        "debug.signal.select.thread1.lock.acquired",
    ),
    (
        "debug.signal.select.thread1.lock.release",
        "debug.signal.select.thread1.lock.release",
    ),
    (
        "debug.signal.select.owner.upgrade.request",
        "debug.signal.select.owner.upgrade.request",
    ),
    (
        "debug.signal.select.owner.upgrade.done",
        "debug.signal.select.owner.upgrade.done",
    ),
    (
        "debug.signal.select.owner.upgrade.miss",
        "debug.signal.select.owner.upgrade.miss",
    ),
    (
        "debug.signal.select.proc.lock.request",
        "debug.signal.select.proc.lock.request",
    ),
    (
        "debug.signal.select.proc.lock.acquired",
        "debug.signal.select.proc.lock.acquired",
    ),
    (
        "debug.signal.select.proc.lock.release",
        "debug.signal.select.proc.lock.release",
    ),
    (
        "debug.signal.select.thread2.lock.request",
        "debug.signal.select.thread2.lock.request",
    ),
    (
        "debug.signal.select.thread2.lock.acquired",
        "debug.signal.select.thread2.lock.acquired",
    ),
    (
        "debug.signal.select.thread2.lock.release",
        "debug.signal.select.thread2.lock.release",
    ),
    (
        "debug.signal.select.thread_pending.hit",
        "debug.signal.select.thread_pending.hit",
    ),
    (
        "debug.signal.select.group_pending.hit",
        "debug.signal.select.group_pending.hit",
    ),
    ("debug.signal.select.done", "debug.signal.select.done"),
    (
        "debug.lock_service.process.payload.exit_group.shm_detach.duration_ns",
        "debug.lock_service.process.payload.exit_group.shm_detach.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.exit_group.drain_fds.duration_ns",
        "debug.lock_service.process.payload.exit_group.drain_fds.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.exit_group.threads_drain.duration_ns",
        "debug.lock_service.process.payload.exit_group.threads_drain.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.exit_group.zombify_threads.duration_ns",
        "debug.lock_service.process.payload.exit_group.zombify_threads.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.exit_group.drop_drained.duration_ns",
        "debug.lock_service.process.payload.exit_group.drop_drained.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.exit_group.payload_drop.duration_ns",
        "debug.lock_service.process.payload.exit_group.payload_drop.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.process_exit.shm_detach.duration_ns",
        "debug.lock_service.process.payload.process_exit.shm_detach.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.process_exit.drain_fds.duration_ns",
        "debug.lock_service.process.payload.process_exit.drain_fds.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns",
        "debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.process_exit.payload_drop.duration_ns",
        "debug.lock_service.process.payload.process_exit.payload_drop.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns",
        "debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.thread_exit.thread_count.duration_ns",
        "debug.lock_service.process.payload.thread_exit.thread_count.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.thread_exit.group_exit.duration_ns",
        "debug.lock_service.process.payload.thread_exit.group_exit.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.robust.head_reads.duration_ns",
        "debug.lock_service.process.payload.robust.head_reads.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.robust.entries.duration_ns",
        "debug.lock_service.process.payload.robust.entries.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.robust.pending.duration_ns",
        "debug.lock_service.process.payload.robust.pending.duration_ns",
    ),
    (
        "debug.lock_service.process.payload.robust.entry_count",
        "debug.lock_service.process.payload.robust.entry_count",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.payload_lock_wait.duration_ns",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.payload_lock_held.duration_ns",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.payload_cap_clone.duration_ns",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.payload_missing",
        "debug.lock_service.thread.payload.sigprocmask.payload_missing",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.mask_compute.duration_ns",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.mask_noop",
        "debug.lock_service.thread.payload.sigprocmask.mask_noop",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.mask_store.duration_ns",
    ),
    (
        "debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns",
        "debug.lock_service.thread.payload.sigprocmask.refresh.duration_ns",
    ),
    (
        "debug.cap.upgrade.to_cap.duration_ns",
        "debug.cap.upgrade.to_cap.duration_ns",
    ),
    (
        "debug.cap.upgrade.to_cap.attempts",
        "debug.cap.upgrade.to_cap.attempts",
    ),
    (
        "debug.cap.upgrade.to_cap.retries",
        "debug.cap.upgrade.to_cap.retries",
    ),
    (
        "debug.cap.upgrade.weak.total.duration_ns",
        "debug.cap.upgrade.weak.total.duration_ns",
    ),
    (
        "debug.cap.upgrade.weak.registry.duration_ns",
        "debug.cap.upgrade.weak.registry.duration_ns",
    ),
    (
        "debug.cap.upgrade.weak.meta.duration_ns",
        "debug.cap.upgrade.weak.meta.duration_ns",
    ),
    (
        "debug.cap.upgrade.weak.to_cap.duration_ns",
        "debug.cap.upgrade.weak.to_cap.duration_ns",
    ),
    (
        "debug.cap.upgrade.weak.outcome",
        "debug.cap.upgrade.weak.outcome",
    ),
    ("debug.cap.upgrade.weak.kind", "debug.cap.upgrade.weak.kind"),
    ("debug.sigprocmask.enter", "debug.sigprocmask.enter"),
    ("debug.sigprocmask.args", "debug.sigprocmask.args"),
    ("debug.sigprocmask.bad_size", "debug.sigprocmask.bad_size"),
    ("debug.sigprocmask.read.err", "debug.sigprocmask.read.err"),
    (
        "debug.sigprocmask.read.after",
        "debug.sigprocmask.read.after",
    ),
    (
        "debug.sigprocmask.step.after",
        "debug.sigprocmask.step.after",
    ),
    (
        "debug.sigprocmask.step.zombie",
        "debug.sigprocmask.step.zombie",
    ),
    (
        "debug.sigprocmask.mask.zombie",
        "debug.sigprocmask.mask.zombie",
    ),
    (
        "debug.sigprocmask.mask.after",
        "debug.sigprocmask.mask.after",
    ),
    ("debug.sigprocmask.write.err", "debug.sigprocmask.write.err"),
    (
        "debug.sigprocmask.write.after",
        "debug.sigprocmask.write.after",
    ),
    ("debug.sigprocmask.return", "debug.sigprocmask.return"),
    ("debug.vm.user.copy_in.len", "debug.vm.user.copy_in.len"),
    ("debug.vm.user.copy_in.phase", "debug.vm.user.copy_in.phase"),
    ("debug.vm.user.copy_in.err", "debug.vm.user.copy_in.err"),
    ("debug.vm.user.copy_in.chunk", "debug.vm.user.copy_in.chunk"),
    (
        "debug.vm.user.copy_in.copied",
        "debug.vm.user.copy_in.copied",
    ),
    (
        "debug.vm.user.copy_in.blocked",
        "debug.vm.user.copy_in.blocked",
    ),
    ("debug.vm.user.resolve.kind", "debug.vm.user.resolve.kind"),
    ("debug.vm.user.resolve.phase", "debug.vm.user.resolve.phase"),
    ("debug.vm.user.resolve.err", "debug.vm.user.resolve.err"),
    (
        "debug.vm.user.resolve.blocked",
        "debug.vm.user.resolve.blocked",
    ),
    (
        "debug.vm.user.resolve.backing",
        "debug.vm.user.resolve.backing",
    ),
    (
        "debug.vm.user.resolve_page.backing",
        "debug.vm.user.resolve_page.backing",
    ),
    (
        "debug.vm.user.resolve_page.err",
        "debug.vm.user.resolve_page.err",
    ),
    (
        "debug.vm.user.resolve_page.phase",
        "debug.vm.user.resolve_page.phase",
    ),
    (
        "debug.vm.user.pagebacked.phase",
        "debug.vm.user.pagebacked.phase",
    ),
    (
        "debug.vm.user.pagebacked.err",
        "debug.vm.user.pagebacked.err",
    ),
    (
        "debug.vm.user.pagebacked.blocked",
        "debug.vm.user.pagebacked.blocked",
    ),
    ("resume", "resume"),
    ("step", "step"),
    ("yield.OnWaitSource", "yield.OnWaitSource"),
    ("yield.OnAgent", "yield.OnAgent"),
    ("yield.OnTimer", "yield.OnTimer"),
    ("wake.notify", "wake.notify"),
    ("debug.trap.syscall", "debug.trap.syscall"),
    ("debug.trap.timer_user", "debug.trap.timer_user"),
    ("debug.trap.handoff.payload", "debug.trap.handoff.payload"),
    ("debug.trap.handoff.active", "debug.trap.handoff.active"),
    ("debug.trap.handoff.capture", "debug.trap.handoff.capture"),
    ("debug.trap.handoff.store", "debug.trap.handoff.store"),
    ("debug.trap.handoff.complete", "debug.trap.handoff.complete"),
    (
        "debug.thread.entry.prepare.before",
        "debug.thread.entry.prepare.before",
    ),
    (
        "debug.thread.entry.prepare.after",
        "debug.thread.entry.prepare.after",
    ),
    ("debug.thread.loop.top", "debug.thread.loop.top"),
    (
        "debug.thread.start_request.after",
        "debug.thread.start_request.after",
    ),
    (
        "debug.thread.active_request.after",
        "debug.thread.active_request.after",
    ),
    ("debug.thread.ast.before", "debug.thread.ast.before"),
    ("debug.thread.ast.after", "debug.thread.ast.after"),
    (
        "debug.thread.checkpoint.after",
        "debug.thread.checkpoint.after",
    ),
    ("debug.thread.enter.before", "debug.thread.enter.before"),
    ("debug.thread.trap.consumed", "debug.thread.trap.consumed"),
    ("debug.thread.await.ready", "debug.thread.await.ready"),
    ("debug.thread.process.after", "debug.thread.process.after"),
    ("debug.thread.aspace.after", "debug.thread.aspace.after"),
    (
        "debug.thread.ctx.process_clone.after",
        "debug.thread.ctx.process_clone.after",
    ),
    (
        "debug.thread.ctx.thread_clone.after",
        "debug.thread.ctx.thread_clone.after",
    ),
    (
        "debug.thread.ctx.aspace_clone.after",
        "debug.thread.ctx.aspace_clone.after",
    ),
    (
        "debug.thread.ctx.cred_snapshot.after",
        "debug.thread.ctx.cred_snapshot.after",
    ),
    ("debug.thread.ctx.after", "debug.thread.ctx.after"),
    ("debug.thread.mailbox.after", "debug.thread.mailbox.after"),
    ("debug.thread.timer.after", "debug.thread.timer.after"),
    ("debug.thread.delegate.after", "debug.thread.delegate.after"),
    (
        "debug.thread.saved_ctx.after",
        "debug.thread.saved_ctx.after",
    ),
    (
        "debug.thread.dispatch.before",
        "debug.thread.dispatch.before",
    ),
    ("debug.thread.dispatch.after", "debug.thread.dispatch.after"),
    (
        "debug.thread.immediate.after",
        "debug.thread.immediate.after",
    ),
    ("debug.thread.oneshot.after", "debug.thread.oneshot.after"),
    ("debug.thread.return.stored", "debug.thread.return.stored"),
    ("debug.clone.enter", "debug.clone.enter"),
    ("debug.clone_thread.enter", "debug.clone_thread.enter"),
    (
        "debug.clone_thread.allocate_tid.after",
        "debug.clone_thread.allocate_tid.after",
    ),
    (
        "debug.clone_thread.sign_thread.after",
        "debug.clone_thread.sign_thread.after",
    ),
    (
        "debug.clone_thread.payload_fresh.after",
        "debug.clone_thread.payload_fresh.after",
    ),
    (
        "debug.clone_thread.payload_sign.after",
        "debug.clone_thread.payload_sign.after",
    ),
    (
        "debug.clone_thread.payload_cap.after",
        "debug.clone_thread.payload_cap.after",
    ),
    (
        "debug.clone_thread.identity_sign.after",
        "debug.clone_thread.identity_sign.after",
    ),
    (
        "debug.clone_thread.register_tid.after",
        "debug.clone_thread.register_tid.after",
    ),
    (
        "debug.clone_thread.seed_context.after",
        "debug.clone_thread.seed_context.after",
    ),
    (
        "debug.clone_thread.clear_ctid.after",
        "debug.clone_thread.clear_ctid.after",
    ),
    (
        "debug.clone_thread.attach.after",
        "debug.clone_thread.attach.after",
    ),
    (
        "debug.clone.parent_ctx.after",
        "debug.clone.parent_ctx.after",
    ),
    (
        "debug.clone.step_thread.after",
        "debug.clone.step_thread.after",
    ),
    (
        "debug.clone.parent_settid.after",
        "debug.clone.parent_settid.after",
    ),
    (
        "debug.clone.reactor_submit.before",
        "debug.clone.reactor_submit.before",
    ),
    (
        "debug.clone.reactor_submit.after",
        "debug.clone.reactor_submit.after",
    ),
    ("debug.clone.return", "debug.clone.return"),
    ("debug.child_submit.enter", "debug.child_submit.enter"),
    (
        "debug.child_submit.payload.after",
        "debug.child_submit.payload.after",
    ),
    (
        "debug.child_submit.payload_clone.after",
        "debug.child_submit.payload_clone.after",
    ),
    (
        "debug.child_submit.reactor.with.before",
        "debug.child_submit.reactor.with.before",
    ),
    (
        "debug.child_submit.submit_call.before",
        "debug.child_submit.submit_call.before",
    ),
    (
        "debug.child_submit.reactor.with.after",
        "debug.child_submit.reactor.with.after",
    ),
    (
        "debug.child_submit.register.after",
        "debug.child_submit.register.after",
    ),
    ("debug.observe.ap.init", "debug.observe.ap.init"),
    (
        "debug.task.submit.slot.after",
        "debug.task.submit.slot.after",
    ),
    (
        "debug.task.submit.future_size",
        "debug.task.submit.future_size",
    ),
    (
        "debug.task.submit.future_box.after",
        "debug.task.submit.future_box.after",
    ),
    (
        "debug.task.submit.wake_state.after",
        "debug.task.submit.wake_state.after",
    ),
    (
        "debug.task.submit.mailbox.after",
        "debug.task.submit.mailbox.after",
    ),
    (
        "debug.task.submit.construct.after",
        "debug.task.submit.construct.after",
    ),
    (
        "debug.task.submit.store.after",
        "debug.task.submit.store.after",
    ),
    (
        "debug.reactor.submit.task_table.after",
        "debug.reactor.submit.task_table.after",
    ),
    (
        "debug.reactor.submit.scheduler.after",
        "debug.reactor.submit.scheduler.after",
    ),
    (
        "debug.reactor.submit.enqueue.after",
        "debug.reactor.submit.enqueue.after",
    ),
    (
        "debug.reactor.submit.from_hart.after",
        "debug.reactor.submit.from_hart.after",
    ),
    (
        "debug.reactor.submit.dispatch.after",
        "debug.reactor.submit.dispatch.after",
    ),
    ("debug.write.enter", "debug.write.enter"),
    ("debug.write.len", "debug.write.len"),
    ("debug.writev.enter", "debug.writev.enter"),
    ("debug.writev.iovcnt", "debug.writev.iovcnt"),
    (
        "debug.futex.step_wait_publish",
        "debug.futex.step_wait_publish",
    ),
    (
        "debug.futex.step_wait_eagain",
        "debug.futex.step_wait_eagain",
    ),
    ("debug.futex.step_wake_hit", "debug.futex.step_wake_hit"),
    ("debug.futex.step_wake_miss", "debug.futex.step_wake_miss"),
    (
        "debug.futex.wait_table.entries",
        "debug.futex.wait_table.entries",
    ),
    (
        "debug.futex.wait_table.waiters_total",
        "debug.futex.wait_table.waiters_total",
    ),
    (
        "debug.futex.wait_table.sample.uaddr",
        "debug.futex.wait_table.sample.uaddr",
    ),
    (
        "debug.futex.wait_table.sample.waiters",
        "debug.futex.wait_table.sample.waiters",
    ),
    (
        "debug.futex.wait_table.sample.mask",
        "debug.futex.wait_table.sample.mask",
    ),
    (
        "debug.futex.wait_table.sample.source",
        "debug.futex.wait_table.sample.source",
    ),
    (
        "debug.futex.wait_table.sample.subscribers",
        "debug.futex.wait_table.sample.subscribers",
    ),
    (
        "debug.futex.wait_table.target_uaddr",
        "debug.futex.wait_table.target_uaddr",
    ),
    (
        "debug.futex.wake_table.entries",
        "debug.futex.wake_table.entries",
    ),
    (
        "debug.futex.wake_table.waiters_total",
        "debug.futex.wake_table.waiters_total",
    ),
    (
        "debug.futex.wake_table.sample.uaddr",
        "debug.futex.wake_table.sample.uaddr",
    ),
    (
        "debug.futex.wake_table.sample.waiters",
        "debug.futex.wake_table.sample.waiters",
    ),
    (
        "debug.futex.wake_table.sample.mask",
        "debug.futex.wake_table.sample.mask",
    ),
    (
        "debug.futex.wake_table.sample.source",
        "debug.futex.wake_table.sample.source",
    ),
    (
        "debug.futex.wake_table.sample.subscribers",
        "debug.futex.wake_table.sample.subscribers",
    ),
    (
        "debug.futex.wake_table.target_uaddr",
        "debug.futex.wake_table.target_uaddr",
    ),
    (
        "debug.futex.cancel_table.entries",
        "debug.futex.cancel_table.entries",
    ),
    (
        "debug.futex.cancel_table.waiters_total",
        "debug.futex.cancel_table.waiters_total",
    ),
    (
        "debug.futex.cancel_table.sample.uaddr",
        "debug.futex.cancel_table.sample.uaddr",
    ),
    (
        "debug.futex.cancel_table.sample.waiters",
        "debug.futex.cancel_table.sample.waiters",
    ),
    (
        "debug.futex.cancel_table.sample.mask",
        "debug.futex.cancel_table.sample.mask",
    ),
    (
        "debug.futex.cancel_table.sample.source",
        "debug.futex.cancel_table.sample.source",
    ),
    (
        "debug.futex.cancel_table.sample.subscribers",
        "debug.futex.cancel_table.sample.subscribers",
    ),
    (
        "debug.futex.cancel_table.target_uaddr",
        "debug.futex.cancel_table.target_uaddr",
    ),
    (
        "debug.futex.requeue_table.entries",
        "debug.futex.requeue_table.entries",
    ),
    (
        "debug.futex.requeue_table.waiters_total",
        "debug.futex.requeue_table.waiters_total",
    ),
    (
        "debug.futex.requeue_table.sample.uaddr",
        "debug.futex.requeue_table.sample.uaddr",
    ),
    (
        "debug.futex.requeue_table.sample.waiters",
        "debug.futex.requeue_table.sample.waiters",
    ),
    (
        "debug.futex.requeue_table.sample.mask",
        "debug.futex.requeue_table.sample.mask",
    ),
    (
        "debug.futex.requeue_table.sample.source",
        "debug.futex.requeue_table.sample.source",
    ),
    (
        "debug.futex.requeue_table.sample.subscribers",
        "debug.futex.requeue_table.sample.subscribers",
    ),
    (
        "debug.futex.requeue_table.target_uaddr",
        "debug.futex.requeue_table.target_uaddr",
    ),
    (
        "debug.futex.wake_decision.uaddr",
        "debug.futex.wake_decision.uaddr",
    ),
    (
        "debug.futex.wake_decision.requested",
        "debug.futex.wake_decision.requested",
    ),
    (
        "debug.futex.wake_decision.waiters_before",
        "debug.futex.wake_decision.waiters_before",
    ),
    (
        "debug.futex.wake_decision.fired_mask",
        "debug.futex.wake_decision.fired_mask",
    ),
    (
        "debug.futex.wake_decision.woken",
        "debug.futex.wake_decision.woken",
    ),
    (
        "debug.futex.wake_decision.posted",
        "debug.futex.wake_decision.posted",
    ),
    (
        "debug.futex.wake_decision.subscribers_before",
        "debug.futex.wake_decision.subscribers_before",
    ),
    (
        "debug.futex.wake_decision.subscribers_after",
        "debug.futex.wake_decision.subscribers_after",
    ),
    (
        "debug.futex.clear_child_tid.uaddr",
        "debug.futex.clear_child_tid.uaddr",
    ),
    (
        "debug.futex.clear_child_tid.woken",
        "debug.futex.clear_child_tid.woken",
    ),
    (
        "debug.futex.clear_child_tid.err",
        "debug.futex.clear_child_tid.err",
    ),
    (
        "debug.futex.clear_child_tid.pending",
        "debug.futex.clear_child_tid.pending",
    ),
    ("debug.sched.submit.queue", "debug.sched.submit.queue"),
    ("debug.sched.pick.queue", "debug.sched.pick.queue"),
    ("debug.sched.runnable.queue", "debug.sched.runnable.queue"),
    ("debug.sched.runnable.front", "debug.sched.runnable.front"),
    ("debug.sched.runnable.hint", "debug.sched.runnable.hint"),
    ("debug.sched.stop.reason", "debug.sched.stop.reason"),
    ("debug.wake.pending_hint", "debug.wake.pending_hint"),
    ("debug.wake.drain_hint", "debug.wake.drain_hint"),
];

/// Linux RV64 generic ABI syscall number → kernel-name table. Picked from
/// `asm-generic/unistd.h` constrained to numbers tx-shims actually wires
/// (so the trace daemon doesn't show every kernel ABI it might call).
fn linux_rv64_syscalls() -> &'static [(u16, &'static str)] {
    &[
        (17, "getcwd"),
        (23, "dup"),
        (24, "dup3"),
        (25, "fcntl"),
        (29, "ioctl"),
        (32, "flock"),
        (34, "mkdirat"),
        (35, "unlinkat"),
        (38, "renameat"),
        (39, "umount2"),
        (40, "mount"),
        (45, "truncate"),
        (46, "ftruncate"),
        (48, "faccessat"),
        (49, "chdir"),
        (56, "openat"),
        (57, "close"),
        (59, "pipe2"),
        (61, "getdents64"),
        (62, "lseek"),
        (63, "read"),
        (64, "write"),
        (66, "writev"),
        (71, "sendfile"),
        (72, "pselect6"),
        (73, "ppoll"),
        (78, "readlinkat"),
        (79, "fstatat"),
        (80, "fstat"),
        (88, "utimensat"),
        (93, "exit"),
        (94, "exit_group"),
        (96, "set_tid_address"),
        (98, "futex"),
        (99, "set_robust_list"),
        (101, "nanosleep"),
        (113, "clock_gettime"),
        (122, "sched_getaffinity"),
        (123, "sched_setaffinity"),
        (124, "sched_yield"),
        (129, "kill"),
        (130, "tkill"),
        (131, "tgkill"),
        (132, "sigaltstack"),
        (133, "rt_sigsuspend"),
        (134, "rt_sigaction"),
        (135, "rt_sigprocmask"),
        (139, "rt_sigreturn"),
        (153, "times"),
        (154, "setpgid"),
        (155, "getpgid"),
        (156, "getsid"),
        (157, "setsid"),
        (160, "uname"),
        (165, "getrusage"),
        (166, "umask"),
        (167, "prctl"),
        (169, "gettimeofday"),
        (172, "getpid"),
        (173, "getppid"),
        (174, "getuid"),
        (175, "geteuid"),
        (176, "getgid"),
        (177, "getegid"),
        (178, "gettid"),
        (179, "sysinfo"),
        (214, "brk"),
        (215, "munmap"),
        (220, "clone"),
        (221, "execve"),
        (222, "mmap"),
        (226, "mprotect"),
        (233, "madvise"),
        (260, "wait4"),
        (261, "prlimit64"),
        (278, "getrandom"),
    ]
}

// ── extract ───────────────────────────────────────────────────────────────────

/// Recover a `.txtrace` blob from a kernel serial log.
///
/// The kernel-side `tx_observe::dump_console_hex*::<P>` helpers emit a
/// compact txtrace snapshot (a synthesised `TxTraceHeader` plus one or more
/// hart rings) over the console as hex bytes framed by
/// `TXTRACE-BEGIN ... TXTRACE-END` sentinels. This subcommand greps the framed
/// hex out of the captured serial log, hex-decodes it, and writes a standalone
/// `.txtrace` file the rest of the pipeline (`validate` / `replay` /
/// `pftrace`) consumes unchanged.
///
/// Usage: `cargo xtask observe extract --serial <log> --output <txtrace>`
fn observe_extract(args: &[String]) -> Result<()> {
    let serial = optional_option_value(args, "--serial")
        .ok_or("--serial is required for extract subcommand")?;
    let output = optional_option_value(args, "--output")
        .ok_or("--output is required for extract subcommand")?;

    let log_path = PathBuf::from(&serial);
    let raw = fs::read_to_string(&log_path)
        .map_err(|e| format!("cannot read serial log {}: {e}", log_path.display()))?;

    // ── Locate framing markers ────────────────────────────────────────────────
    let begin_marker = "TXTRACE-BEGIN ";
    let end_marker = "TXTRACE-END";
    let begin_idx = raw
        .find(begin_marker)
        .ok_or_else(|| "no TXTRACE-BEGIN marker in serial log".to_string())?;
    let end_idx = raw[begin_idx..]
        .find(end_marker)
        .map(|off| begin_idx + off)
        .ok_or_else(|| "TXTRACE-BEGIN present but no matching TXTRACE-END".to_string())?;

    // The header line carries metadata (`bytes=`, `hart=`, `clock_hz=`) —
    // skip past the newline that terminates it.
    let after_header = raw[begin_idx..end_idx]
        .find('\n')
        .map(|off| begin_idx + off + 1)
        .ok_or_else(|| "malformed TXTRACE-BEGIN header line".to_string())?;
    let hex_block = &raw[after_header..end_idx];
    let header_line = raw[begin_idx..raw[begin_idx..].find('\n').map(|o| begin_idx + o).unwrap()]
        .strip_prefix(begin_marker)
        .unwrap_or("");

    // ── Hex-decode (skipping whitespace) ──────────────────────────────────────
    let mut bytes = Vec::with_capacity(hex_block.len() / 2);
    let mut nibble: Option<u8> = None;
    for c in hex_block.chars() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = match c {
            '0'..='9' => c as u8 - b'0',
            'a'..='f' => c as u8 - b'a' + 10,
            'A'..='F' => c as u8 - b'A' + 10,
            _ => {
                return Err(format!(
                    "unexpected non-hex character '{}' inside TXTRACE frame",
                    c
                ));
            }
        };
        nibble = match nibble {
            None => Some(v),
            Some(hi) => {
                bytes.push((hi << 4) | v);
                None
            }
        };
    }
    if nibble.is_some() {
        return Err("hex stream has an odd nibble count".into());
    }

    let output_path = PathBuf::from(&output);
    fs::write(&output_path, &bytes)
        .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;
    eprintln!(
        "observe: extracted {} bytes from {} (header: {}) → {}",
        bytes.len(),
        log_path.display(),
        header_line.trim(),
        output_path.display(),
    );
    Ok(())
}

// ── Daemon binary helpers ─────────────────────────────────────────────────────

fn daemon_dir(root: &Path) -> PathBuf {
    root.join("tools/tx-trace-daemon")
}

/// Path to the compiled `tx-trace-daemon` binary.
///
/// The daemon lives in its own workspace under `tools/tx-trace-daemon/`, but
/// the root `.cargo/config.toml` sets `target-dir = "target"` (an absolute-or-
/// relative path resolved from each workspace's root).  When cargo processes the
/// daemon workspace it inherits the parent `.cargo/config.toml` and therefore
/// places outputs in the *root* workspace `target/` directory, not in a
/// `tools/tx-trace-daemon/target/` subdirectory.
fn daemon_bin(root: &Path) -> PathBuf {
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|p| if p.is_absolute() { p } else { root.join(p) })
        .unwrap_or_else(|| root.join("target"));
    target_dir.join("debug/tx-trace-daemon")
}

/// Ensure `tx-trace-daemon` binary is built.
///
/// Drives `cargo build` from the daemon's own workspace directory so that its
/// independent `Cargo.toml` / `[workspace]` is respected, while the inherited
/// `.cargo/config.toml` `target-dir = "target"` still routes the output to the
/// root `target/` directory.
fn ensure_daemon_built(root: &Path) -> Result<()> {
    if daemon_bin(root).exists() {
        // Already built; skip rebuild for speed in repeated invocations.
        return Ok(());
    }
    eprintln!("observe: building tx-trace-daemon ...");
    let status = Command::new("cargo")
        .args(["build"])
        .current_dir(daemon_dir(root))
        .status()
        .map_err(|err| format!("failed to run cargo build for tx-trace-daemon: {err}"))?;
    if !status.success() {
        return Err("tx-trace-daemon build failed".into());
    }
    let bin = daemon_bin(root);
    if !bin.exists() {
        return Err(format!(
            "tx-trace-daemon binary not found at {} after build; \
             check that the daemon workspace's Cargo.toml is correct",
            bin.display()
        ));
    }
    Ok(())
}

// ── replay ────────────────────────────────────────────────────────────────────

fn observe_replay(root: &Path, args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let out_format = optional_option_value(args, "--out").unwrap_or_else(|| "json".to_string());
    let output = optional_option_value(args, "--output");
    let filter = optional_option_value(args, "--filter");

    ensure_daemon_built(root)?;

    let mut cmd_args = vec![
        "replay".to_string(),
        "--file".to_string(),
        file.to_string_lossy().into_owned(),
        "--out".to_string(),
        out_format.clone(),
    ];

    if let Some(out) = &output {
        cmd_args.push("--output".to_string());
        cmd_args.push(out.clone());
    }
    if let Some(f) = &filter {
        cmd_args.push("--filter".to_string());
        cmd_args.push(f.clone());
    }

    if out_format == "pftrace" && output.is_none() {
        return Err("--output is required when --out pftrace".into());
    }

    eprintln!(
        "observe: tx-trace-daemon replay --file {} --out {} ...",
        file.display(),
        out_format,
    );

    let status = Command::new(daemon_bin(root))
        .args(&cmd_args)
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

// ── analyze ──────────────────────────────────────────────────────────────────

fn observe_analyze(root: &Path, args: &[String]) -> Result<()> {
    if !command_exists("python3") {
        return Err("python3 is required for observe analyze".into());
    }

    let script = root.join("tools").join("tx-observe-analyze.py");
    if !script.exists() {
        return Err(format!("analyzer script not found: {}", script.display()));
    }
    let mut py_args = vec![script.display().to_string()];
    py_args.extend(args.iter().cloned());
    run_cmd_owned(root, "python3", &py_args)
}

// ── pftrace ───────────────────────────────────────────────────────────────────

fn observe_pftrace(root: &Path, args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let output = optional_option_value(args, "--output")
        .ok_or("--output is required for pftrace subcommand")?;

    ensure_daemon_built(root)?;

    // Resolve a names.json with this precedence (per OBS-V1-OPNAME-1):
    //   1. explicit `--names <path>`
    //   2. sibling `<txtrace_stem>.names.json` next to the input
    //   3. auto-generate next to the input via `observe names` if
    //      `--kernel <elf>` is supplied (or sibling kernel-rv exists)
    //
    // Each fallback keeps the same `--names` shape so the daemon doesn't
    // see the difference.
    let explicit_names = optional_option_value(args, "--names");
    let sibling_names = file.with_extension("names.json");
    let names_path = if let Some(p) = explicit_names {
        Some(PathBuf::from(p))
    } else if sibling_names.exists() {
        Some(sibling_names.clone())
    } else if let Some(kernel) = resolve_names_kernel_elf(root, args) {
        // Auto-generate the sibling names.json from the kernel ELF's
        // symbol table. Cheap (<100 ms on rv64-qemu's 42 MiB ELF).
        eprintln!(
            "observe: auto-generating names.json from {} -> {}",
            kernel.display(),
            sibling_names.display(),
        );
        let gen_args = vec![
            "--kernel".to_string(),
            kernel.display().to_string(),
            "--output".to_string(),
            sibling_names.display().to_string(),
        ];
        observe_names(&gen_args)?;
        Some(sibling_names)
    } else {
        None
    };

    eprintln!(
        "observe: tx-trace-daemon replay --file {} --out pftrace --output {}{} ...",
        file.display(),
        output,
        names_path
            .as_deref()
            .map(|p| format!(" --names {}", p.display()))
            .unwrap_or_default(),
    );

    let mut cmd = Command::new(daemon_bin(root));
    cmd.args([
        "replay",
        "--file",
        &file.to_string_lossy(),
        "--out",
        "pftrace",
        "--output",
        &output,
    ]);
    if let Some(p) = names_path.as_deref() {
        cmd.args(["--names", &p.to_string_lossy()]);
    }
    let status = cmd
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

// ── bundle ───────────────────────────────────────────────────────────────────

fn observe_bundle(root: &Path, args: &[String]) -> Result<()> {
    let out_dir = optional_option_value(args, "--output-dir")
        .map(PathBuf::from)
        .ok_or("--output-dir <dir> is required for bundle subcommand")?;
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("failed to create {}: {e}", out_dir.display()))?;

    let file_arg = optional_option_value(args, "--file").map(PathBuf::from);
    let serial_arg = optional_option_value(args, "--serial").map(PathBuf::from);
    let file = match (file_arg, serial_arg) {
        (Some(file), None) => file,
        (None, Some(serial)) => {
            let serial_copy = out_dir.join("serial.txt");
            if serial != serial_copy {
                fs::copy(&serial, &serial_copy).map_err(|e| {
                    format!(
                        "failed to copy serial log {} -> {}: {e}",
                        serial.display(),
                        serial_copy.display()
                    )
                })?;
            }
            let trace = out_dir.join("trace.txtrace");
            let extract_args = vec![
                "--serial".to_string(),
                serial_copy.display().to_string(),
                "--output".to_string(),
                trace.display().to_string(),
            ];
            observe_extract(&extract_args)?;
            trace
        }
        (Some(_), Some(_)) => {
            return Err("bundle accepts either --file or --serial, not both".into());
        }
        (None, None) => {
            return Err("bundle requires --file <txtrace> or --serial <log>".into());
        }
    };

    ensure_daemon_built(root)?;

    let explicit_names = optional_option_value(args, "--names");
    let sibling_names = file.with_extension("names.json");
    let out_names = out_dir.join("names.json");
    let names_path = if let Some(p) = explicit_names {
        Some(PathBuf::from(p))
    } else if sibling_names.exists() {
        Some(sibling_names)
    } else if let Some(kernel) = resolve_names_kernel_elf(root, args) {
        eprintln!(
            "observe: auto-generating names.json from {} -> {}",
            kernel.display(),
            out_names.display(),
        );
        let gen_args = vec![
            "--kernel".to_string(),
            kernel.display().to_string(),
            "--output".to_string(),
            out_names.display().to_string(),
        ];
        observe_names(&gen_args)?;
        Some(out_names)
    } else {
        None
    };

    let mut cmd = Command::new(daemon_bin(root));
    cmd.args([
        "bundle",
        "--file",
        &file.to_string_lossy(),
        "--out-dir",
        &out_dir.to_string_lossy(),
    ]);
    if let Some(names) = names_path.as_deref() {
        cmd.args(["--names", &names.to_string_lossy()]);
    }

    eprintln!(
        "observe: tx-trace-daemon bundle --file {} --out-dir {}{} ...",
        file.display(),
        out_dir.display(),
        names_path
            .as_deref()
            .map(|p| format!(" --names {}", p.display()))
            .unwrap_or_default(),
    );

    let status = cmd
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

// ── live guest memory ─────────────────────────────────────────────────────────

fn observe_live_guest_mem(root: &Path, args: &[String]) -> Result<()> {
    let guest_mem = optional_option_value(args, "--guest-mem")
        .ok_or("--guest-mem <ram-file> is required for live-guest-mem")?;
    let kernel = optional_option_value(args, "--kernel")
        .ok_or("--kernel <elf> is required for live-guest-mem")?;
    let out_dir = optional_option_value(args, "--output-dir")
        .ok_or("--output-dir <dir> is required for live-guest-mem")?;

    ensure_daemon_built(root)?;

    let mut cmd = Command::new(daemon_bin(root));
    cmd.args([
        "live-guest-mem",
        "--guest-mem",
        &guest_mem,
        "--kernel",
        &kernel,
        "--out-dir",
        &out_dir,
    ]);
    pass_optional_arg(args, &mut cmd, "--symbol");
    pass_optional_arg(args, &mut cmd, "--hart-count");
    pass_optional_arg(args, &mut cmd, "--ring-bytes");
    pass_optional_arg(args, &mut cmd, "--poll-ms");
    pass_optional_arg(args, &mut cmd, "--stop-file");
    pass_optional_arg(args, &mut cmd, "--max-duration-ms");
    pass_optional_arg(args, &mut cmd, "--names");
    pass_optional_flag(args, &mut cmd, "--finalize");

    eprintln!(
        "observe: tx-trace-daemon live-guest-mem --guest-mem {} --kernel {} --out-dir {} ...",
        guest_mem, kernel, out_dir
    );
    let status = cmd
        .status()
        .map_err(|err| format!("failed to run tx-trace-daemon: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tx-trace-daemon exited with {status}"))
    }
}

fn observe_oscomp_live(root: &Path, args: &[String]) -> Result<()> {
    let script = root.join("tools/oscomp-observe-live.py");
    if !script.exists() {
        return Err(format!("missing {}", script.display()));
    }
    let status = Command::new("python3")
        .arg(script)
        .args(args)
        .status()
        .map_err(|err| format!("failed to run oscomp live observe workflow: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("oscomp live observe workflow exited with {status}"))
    }
}

fn pass_optional_arg(args: &[String], cmd: &mut Command, name: &str) {
    if let Some(value) = optional_option_value(args, name) {
        cmd.args([name, &value]);
    }
}

fn pass_optional_flag(args: &[String], cmd: &mut Command, name: &str) {
    if args.iter().any(|arg| arg == name) {
        cmd.arg(name);
    }
}

// ── validate ──────────────────────────────────────────────────────────────────

/// Parse the file header and walk slots without emitting anything.
///
/// Prints a summary line:
///   `txtrace v0: 1 harts, 16 slots/hart, 4 records, 0 framing errors`
fn observe_validate(args: &[String]) -> Result<()> {
    let file = require_file_arg(args)?;
    let data = fs::read(&file).map_err(|err| format!("cannot read {}: {err}", file.display()))?;

    // ── Header constants (mirrors replay.rs constants) ────────────────────────
    const HEADER_SIZE: usize = 72; // size_of::<TxTraceHeader>()
    const RING_HDR_SIZE: usize = 208; // size_of::<TxTraceHartRing>()
    const RECORD_SIZE: usize = 80; // size_of::<TxTraceRecord>()
    const TX_TRACE_MAGIC: u32 = 0x5254_5854;
    const RECORD_MAGIC: u16 = 0x5254;

    if data.len() < HEADER_SIZE {
        return Err(format!(
            "file too small ({} bytes) for TxTraceHeader",
            data.len()
        ));
    }

    // Read header fields with little-endian byte parsing.
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic != TX_TRACE_MAGIC {
        return Err(format!(
            "bad header magic: expected 0x{TX_TRACE_MAGIC:08x}, got 0x{magic:08x}"
        ));
    }

    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    let rings_off = u64::from_le_bytes(data[64..72].try_into().unwrap()) as usize;
    let hart_count = u16::from_le_bytes(data[12..14].try_into().unwrap()) as usize;
    let ring_order = data[14] as u32;
    let record_size_hdr = u16::from_le_bytes(data[10..12].try_into().unwrap()) as usize;

    if record_size_hdr != RECORD_SIZE {
        return Err(format!(
            "unsupported record_size {record_size_hdr}: expected {RECORD_SIZE}"
        ));
    }
    if !(2..=24).contains(&ring_order) {
        return Err(format!(
            "ring_order {ring_order} is outside the valid range [2, 24]"
        ));
    }

    let slot_count = 1usize << ring_order;
    let ring_data_size = RING_HDR_SIZE + slot_count * RECORD_SIZE;
    let required = rings_off + hart_count * ring_data_size;
    if data.len() < required {
        return Err(format!(
            "file too small: need {required} bytes, got {}",
            data.len()
        ));
    }

    // Walk all slots and count records + framing errors.
    let mut total_records = 0usize;
    let mut framing_errors = 0usize;

    // Ring producer/consumer offsets (matching replay.rs RING_PRODUCER_OFF/RING_CONSUMER_OFF)
    const RING_PRODUCER_OFF: usize = 64;
    const RING_CONSUMER_OFF: usize = 128;

    for h in 0..hart_count {
        let ring_base = rings_off + h * ring_data_size;
        let ring_bytes = &data[ring_base..ring_base + RING_HDR_SIZE];

        let producer = u64::from_le_bytes(
            ring_bytes[RING_PRODUCER_OFF..RING_PRODUCER_OFF + 8]
                .try_into()
                .unwrap(),
        );
        let consumer = u64::from_le_bytes(
            ring_bytes[RING_CONSUMER_OFF..RING_CONSUMER_OFF + 8]
                .try_into()
                .unwrap(),
        );
        let effective_consumer = if producer.wrapping_sub(consumer) > slot_count as u64 {
            producer.wrapping_sub(slot_count as u64)
        } else {
            consumer
        };

        let slots_base = ring_base + RING_HDR_SIZE;
        let mut c = effective_consumer;
        while c != producer {
            let slot_idx = (c & (slot_count as u64 - 1)) as usize;
            let slot_off = slots_base + slot_idx * RECORD_SIZE;
            let slot_bytes = &data[slot_off..slot_off + RECORD_SIZE];
            let rec_magic = u16::from_le_bytes(slot_bytes[0..2].try_into().unwrap());
            if rec_magic == RECORD_MAGIC {
                total_records += 1;
            } else {
                framing_errors += 1;
            }
            c = c.wrapping_add(1);
        }
    }

    println!(
        "txtrace v{version}: {hart_count} hart{}, {slot_count} slots/hart, {total_records} record{}, {framing_errors} framing error{}",
        if hart_count == 1 { "" } else { "s" },
        if total_records == 1 { "" } else { "s" },
        if framing_errors == 1 { "" } else { "s" },
    );
    Ok(())
}

// ── demo ──────────────────────────────────────────────────────────────────────

/// Generate a synthetic `.txtrace` file containing a known mix of records.
///
/// Records emitted (in order):
///   1. SpanBegin  (level=Drive,    name=0x0000_0001, span=0x0000_0001)
///   2. Instant    (level=Boundary, name=0x0000_0002, payload_tag=WaitSourceNotify)
///   3. Counter    (level=Boundary, name=0x0000_0003, payload_tag=CounterValue, value=42)
///   4. SpanEnd    (level=Boundary, name=0x0000_0000, span=0x0000_0001)
///
/// With `--with-yields`, additionally appends:
///   5. Instant    (level=Yield,    name=0x0000_0004, payload_tag=YieldBegin)
///   6. Instant    (level=Yield,    name=0x0000_0005, payload_tag=Resume)
///
/// The file is valid `txtrace v0` and round-trips through `observe validate`
/// and `observe replay --out json`.
///
/// Ring layout chosen so slot_count=16 (ring_order=4), 1 hart.
fn observe_demo(args: &[String]) -> Result<()> {
    let output = optional_option_value(args, "--output")
        .ok_or("--output is required for demo subcommand")?;
    let output_path = PathBuf::from(&output);

    let records_arg: Option<usize> =
        optional_option_value(args, "--records").and_then(|v| v.parse().ok());
    let with_yields = args.iter().any(|a| a == "--with-yields");

    // ── Layout constants ──────────────────────────────────────────────────────
    const TX_TRACE_MAGIC: u32 = 0x5254_5854;
    const RECORD_MAGIC: u16 = 0x5254;
    const RECORD_VERSION: u8 = 0;
    const HEADER_SIZE: usize = 72;
    const RING_HDR_SIZE: usize = 208;
    const RECORD_SIZE: usize = 80;
    const RING_ORDER: u8 = 4; // 16 slots
    const SLOT_COUNT: usize = 1 << RING_ORDER;
    const RINGS_OFF: usize = HEADER_SIZE;
    const RING_PRODUCER_OFF: usize = 64;

    // ── FNV-1a 32 ── matches `op_name_id` in tx-scripts/src/drive.rs so the
    // daemon's intern table resolves the synthetic name ids the same way it
    // resolves production-emitted ones.
    fn fnv1a32(s: &str) -> u32 {
        let mut hash: u32 = 0x811c_9dc5;
        for &b in s.as_bytes() {
            hash ^= b as u32;
            hash = hash.wrapping_mul(0x0100_0193);
        }
        hash
    }

    // ── Build a realistic sys_read scenario ───────────────────────────────────
    //
    // Hierarchy (matches the v1 spec's worked example, §9):
    //   L0 SpanBegin sys_read
    //     L2 SpanBegin drive.PipeReadOp
    //       L4 SpanBegin step.iteration_0
    //       L4 SpanEnd   step.iteration_0 (StepOutcome=Yield, OnWaitSource)
    //       L3 SpanBegin yield.OnWaitSource       [with_yields]
    //       L3 Instant   wake.notify              [always — producer side]
    //       L3 Instant   resume                   [with_yields]
    //       L3 SpanEnd   yield.OnWaitSource       [with_yields]
    //       L4 SpanBegin step.iteration_1         [with_yields]
    //       L4 SpanEnd   step.iteration_1 (StepOutcome=Done, 4096 bytes)
    //     L2 SpanEnd   drive.PipeReadOp
    //   L0 SpanEnd   sys_read
    let n_sys_read = fnv1a32("sys_read");
    let n_drive = fnv1a32("drive.tx_subsystems::pipe::PipeReadOp");
    let n_step0 = fnv1a32("step.iteration_0");
    let n_step1 = fnv1a32("step.iteration_1");
    let n_yield = fnv1a32("yield.OnWaitSource");
    let n_wake = fnv1a32("wake.notify");
    let n_resume = fnv1a32("resume");

    let names_table = [
        (n_sys_read, "sys_read"),
        (n_drive, "drive.tx_subsystems::pipe::PipeReadOp"),
        (n_step0, "step.iteration_0"),
        (n_step1, "step.iteration_1"),
        (n_yield, "yield.OnWaitSource"),
        (n_wake, "wake.notify"),
        (n_resume, "resume"),
    ];

    let sys_span: u64 = 0x0000_0000_0000_0001;
    let drv_span: u64 = 0x0000_0000_0000_0002;
    let step0_span: u64 = 0x0000_0000_0000_0003;
    let yield_span: u64 = 0x0000_0000_0000_0004;
    let step1_span: u64 = 0x0000_0000_0000_0005;

    // Helper payload builders (kept inline to avoid bloating the function
    // surface with a payload-builder module).
    let payload_syscall_enter = {
        // PayloadSyscallEnter: sysno=63 (Linux NR_READ rv64), abi=0, argc=6
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&63u32.to_le_bytes());
        p[4..6].copy_from_slice(&0u16.to_le_bytes());
        p[6..8].copy_from_slice(&6u16.to_le_bytes());
        p
    };
    let payload_syscall_exit = {
        // PayloadSyscallExit: ret=4096, errno=0, result_kind=0 (Ok)
        let mut p = [0u8; 16];
        p[0..8].copy_from_slice(&4096i64.to_le_bytes());
        p[8..12].copy_from_slice(&0i32.to_le_bytes());
        p[12] = 0u8; // result_kind
        p
    };
    let payload_drive_begin = {
        // PayloadDriveBegin: op_type, mode=1(Waiting), interrupt=1, has_deadline=0, task_id_low=0x42
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&n_drive.to_le_bytes());
        p[4] = 1u8;
        p[5] = 1u8;
        p[6] = 0u8;
        p[8..12].copy_from_slice(&0x42u32.to_le_bytes());
        p
    };
    let payload_drive_end_ok = {
        // PayloadDriveEnd: ret=0, errno=0, result_kind=0 (Done)
        let mut p = [0u8; 16];
        p[12] = 0u8;
        p
    };
    let payload_step_yield = {
        // PayloadStepOutcome: variant=1 (Yield), progress_empty=1, progress_kind=1 (ByteProgress),
        // shape_kind=1 (OnWaitSource), errno=0, progress_value=0
        let mut p = [0u8; 16];
        p[0] = 1; // variant=Yield
        p[1] = 1; // progress_empty
        p[2] = 1; // progress_kind=Byte
        p[3] = 1; // shape_kind=OnWaitSource
                  // errno (4..8) = 0
                  // progress_value (8..12) = 0
        p
    };
    let payload_step_done = {
        // PayloadStepOutcome: variant=2 (Done), progress_empty=0, progress_kind=1, progress_value=4096
        let mut p = [0u8; 16];
        p[0] = 2; // variant=Done
        p[1] = 0;
        p[2] = 1; // ByteProgress
        p[3] = 0;
        p[8..12].copy_from_slice(&4096u32.to_le_bytes()); // progress_value
        p
    };
    let payload_yield_begin = {
        // PayloadYieldBegin: shape_kind=1, task_id_low=0x42, wait_generation=7
        let mut p = [0u8; 16];
        p[0] = 1;
        p[4..8].copy_from_slice(&0x42u32.to_le_bytes());
        p[8..16].copy_from_slice(&7u64.to_le_bytes());
        p
    };
    let payload_wait_source_notify = {
        // PayloadWaitSourceNotify: source_id_low=0x1234, mask_bits=1, task_id_low=0x42, wait_generation_low=7
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&0x1234u32.to_le_bytes());
        p[4..8].copy_from_slice(&1u32.to_le_bytes());
        p[8..12].copy_from_slice(&0x42u32.to_le_bytes());
        p[12..16].copy_from_slice(&7u32.to_le_bytes());
        p
    };
    let payload_resume = {
        // PayloadResume: resume_kind=0 (Retry), abort_reason=0, object_id_low=0x1234, wait_generation=7
        let mut p = [0u8; 16];
        p[0] = 0;
        p[1] = 0;
        p[4..8].copy_from_slice(&0x1234u32.to_le_bytes());
        p[8..16].copy_from_slice(&7u64.to_le_bytes());
        p
    };

    // Records list — kinds: 10=SpanBegin, 11=SpanEnd, 12=Instant.
    // Levels: 0=Boundary(L0), 2=Drive(L2), 3=Yield(L3), 4=Step(L4).
    // Payload tags: 1=SyscallEnter, 2=SyscallExit, 10=DriveBegin, 11=DriveEnd,
    //               12=StepOutcome, 20=YieldBegin, 21=Resume, 22=WaitSourceNotify.
    let mut record_payloads: Vec<DemoRecord> = vec![
        (10, 0, n_sys_read, sys_span, 0, 1, 8, payload_syscall_enter),
        (
            10,
            2,
            n_drive,
            drv_span,
            sys_span,
            10,
            12,
            payload_drive_begin,
        ),
        (10, 4, n_step0, step0_span, drv_span, 0, 0, [0u8; 16]),
        (11, 4, 0, step0_span, 0, 12, 16, payload_step_yield),
    ];

    if with_yields {
        record_payloads.push((
            10,
            3,
            n_yield,
            yield_span,
            drv_span,
            20,
            16,
            payload_yield_begin,
        ));
    }
    // Producer-side wake.notify always emits (independent of consumer state).
    record_payloads.push((
        12,
        3,
        n_wake,
        0,
        drv_span,
        22,
        16,
        payload_wait_source_notify,
    ));
    if with_yields {
        record_payloads.push((12, 3, n_resume, 0, yield_span, 21, 16, payload_resume));
        record_payloads.push((11, 3, 0, yield_span, 0, 0, 0, [0u8; 16]));
        record_payloads.push((10, 4, n_step1, step1_span, drv_span, 0, 0, [0u8; 16]));
        record_payloads.push((11, 4, 0, step1_span, 0, 12, 16, payload_step_done));
    }
    record_payloads.push((11, 2, 0, drv_span, 0, 11, 16, payload_drive_end_ok));
    record_payloads.push((11, 0, 0, sys_span, 0, 2, 16, payload_syscall_exit));

    // Apply --records override (truncate or pad with Instant records)
    if let Some(n) = records_arg {
        record_payloads.truncate(n);
        while record_payloads.len() < n && record_payloads.len() < SLOT_COUNT {
            record_payloads.push((12u8, 0u8, 0x0000_0099u32, 0u64, 0u64, 0u16, 0u16, [0u8; 16]));
        }
    }

    let num_records = record_payloads.len().min(SLOT_COUNT);
    let total_size = RINGS_OFF + RING_HDR_SIZE + SLOT_COUNT * RECORD_SIZE;
    let mut buf = vec![0u8; total_size];

    // ── TxTraceHeader (72 bytes at offset 0) ──────────────────────────────────
    buf[0..4].copy_from_slice(&TX_TRACE_MAGIC.to_le_bytes());
    buf[4..6].copy_from_slice(&0u16.to_le_bytes()); // version = 0
    buf[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes()); // header_len
    buf[8] = 1u8; // endian = LE
    buf[9] = 8u8; // ptr_width = 8
    buf[10..12].copy_from_slice(&(RECORD_SIZE as u16).to_le_bytes()); // record_size = 80
    buf[12..14].copy_from_slice(&1u16.to_le_bytes()); // hart_count = 1
    buf[14] = RING_ORDER; // ring_order = 4
    buf[15] = 0u8; // flags
                   // _pad0 (16-19) = 0
                   // boot_id (24-31) = 0
                   // clock_id (32-35) = 0 (Unknown)
                   // _pad1 (36-39) = 0
                   // clock_freq_hz (40-47) = 0
                   // string_table_off (48-55) = 0
                   // string_table_len (56-63) = 0
    buf[64..72].copy_from_slice(&(RINGS_OFF as u64).to_le_bytes()); // rings_off

    // ── TxTraceHartRing (208 bytes at offset 72) ──────────────────────────────
    // hart_id = 0, flags = 0 (bytes 0-3 of ring)
    let ring_base = RINGS_OFF;
    // producer at ring_base + RING_PRODUCER_OFF:
    buf[ring_base + RING_PRODUCER_OFF..ring_base + RING_PRODUCER_OFF + 8]
        .copy_from_slice(&(num_records as u64).to_le_bytes());
    // consumer at ring_base + 128 = 0 (already zeroed)

    // ── Record slots (starting at ring_base + RING_HDR_SIZE) ─────────────────
    let slots_base = ring_base + RING_HDR_SIZE;
    for (i, &(kind, level, name, span, parent, payload_tag, payload_len, payload)) in
        record_payloads[..num_records].iter().enumerate()
    {
        let off = slots_base + i * RECORD_SIZE;
        let slot = &mut buf[off..off + RECORD_SIZE];
        // Write all fields at their documented offsets (see record.rs layout).
        slot[0..2].copy_from_slice(&RECORD_MAGIC.to_le_bytes()); // magic
        slot[2] = RECORD_VERSION; // version
        slot[3] = kind; // kind
        slot[4] = level; // level
        slot[5] = 0u8; // flags
        slot[6] = 0u8; // arg_count
        slot[7] = 0u8; // _pad0
        slot[8..10].copy_from_slice(&0u16.to_le_bytes()); // hart = 0
        slot[10..12].copy_from_slice(&0u16.to_le_bytes()); // _pad1
        slot[12..16].copy_from_slice(&0u32.to_le_bytes()); // _pad2
        slot[16..24].copy_from_slice(&(i as u64).to_le_bytes()); // seq
        slot[24..32].copy_from_slice(&((i as u64 + 1) * 1000).to_le_bytes()); // ts
        slot[32..40].copy_from_slice(&span.to_le_bytes()); // span
        slot[40..48].copy_from_slice(&parent.to_le_bytes()); // parent
        slot[48..52].copy_from_slice(&name.to_le_bytes()); // name
        slot[52..54].copy_from_slice(&payload_tag.to_le_bytes()); // payload_tag
        slot[54..56].copy_from_slice(&payload_len.to_le_bytes()); // payload_len
        slot[56..72].copy_from_slice(&payload); // payload
                                                // _pad3 (72-79) = 0 (already zeroed)
    }

    fs::write(&output_path, &buf)
        .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;

    // Sibling `<stem>.names.json` — picked up automatically by
    // `xtask observe pftrace` so the rendered timeline shows human names
    // (`sys_read`, `drive.PipeReadOp`, `step.iteration_0`, …) instead of
    // the `name_0xNNNN` fallback.
    let names_path = output_path.with_extension("names.json");
    let mut json = String::from("{\n  \"name_table\": {\n");
    for (i, (id, name)) in names_table.iter().enumerate() {
        let sep = if i + 1 == names_table.len() { "" } else { "," };
        json.push_str(&format!("    \"{}\": \"{}\"{}\n", id, name, sep));
    }
    json.push_str("  }\n}\n");
    fs::write(&names_path, &json)
        .map_err(|err| format!("failed to write {}: {err}", names_path.display()))?;

    eprintln!(
        "observe: demo file written to {} ({} bytes, {} records{})",
        output_path.display(),
        buf.len(),
        num_records,
        if with_yields { ", with yields" } else { "" }
    );
    eprintln!("observe: names.json written to {}", names_path.display());
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn require_file_arg(args: &[String]) -> Result<PathBuf> {
    let path_str = optional_option_value(args, "--file").ok_or("--file is required")?;
    let path = PathBuf::from(&path_str);
    if !path.exists() {
        return Err(format!(
            "file not found: {}; check the path and try again",
            path.display()
        ));
    }
    Ok(path)
}
