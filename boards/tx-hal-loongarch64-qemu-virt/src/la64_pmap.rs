use super::boot_facts::linked_kernel_image;
use super::la64_irq_trap::{
    align_up, console_write_hex, console_write_literal, la64_timebase_frequency_hz, read_la64_csr,
    write_la64_csr,
};
#[cfg(feature = "la64-boot-trace")]
use super::la64_irq_trap::{console_write_hex, console_write_literal};
use super::la64_percpu::la64_current_cpu_id;
use super::*;
use tx_hal::TlbProgressSpinWait;

static LA64_TLB_STALL_DIAG_EMITTED: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn spin_with_la64_tlb_progress(wait: &mut TlbProgressSpinWait) {
    wait.spin_with(|| {
        service_la64_pending_tlb_shootdown();
    });
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
type La64TestInvtlbAllHook = fn(CpuId);

/// Host-test-only INVTLB completion point used to inject mailbox events
/// before `completed` is published. It is absent from LA64 production builds.
#[cfg(all(test, not(target_arch = "loongarch64")))]
static LA64_TEST_INVTLB_ALL_HOOK: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, not(target_arch = "loongarch64")))]
pub(crate) fn install_la64_test_invtlb_all_hook(hook: Option<La64TestInvtlbAllHook>) {
    LA64_TEST_INVTLB_ALL_HOOK.store(hook.map_or(0, |hook| hook as usize), Ordering::Release);
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn run_la64_test_invtlb_all_hook() {
    let hook = LA64_TEST_INVTLB_ALL_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        let hook = unsafe { core::mem::transmute::<usize, La64TestInvtlbAllHook>(hook) };
        hook(la64_current_cpu_id());
    }
}

pub(crate) fn uart_put_byte(byte: u8) {
    let base = la64_uncached_virt(QEMU_LA64_UART0_BASE) as *mut u8;
    let mut wait = TlbProgressSpinWait::new();

    unsafe {
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_THRE == 0 {
            spin_with_la64_tlb_progress(&mut wait);
        }
        core::ptr::write_volatile(base.add(UART_THR), byte);
    }
}

pub(crate) fn uart_try_get_byte() -> Option<u8> {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let base = la64_uncached_virt(QEMU_LA64_UART0_BASE) as *const u8;
        if core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_DR == 0 {
            return None;
        }

        Some(core::ptr::read_volatile(base.add(UART_RBR)))
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        None
    }
}

pub(crate) const fn la64_cached_virt(phys: usize) -> usize {
    LA64_DMW_CACHED_BASE | phys
}

pub(crate) const fn la64_uncached_virt(phys: usize) -> usize {
    LA64_DMW_UNCACHED_BASE | phys
}

pub(crate) const fn la64_dmw_direct_map() -> VirtRange {
    VirtRange {
        start: VirtAddr(LA64_DMW_CACHED_BASE),
        size: QEMU_LA64_DIRECT_MAP_SIZE,
    }
}

pub(crate) const fn la64_dmw_mapped_phys() -> PhysRange {
    PhysRange {
        start: PhysAddr(QEMU_LA64_RAM_BASE),
        size: QEMU_LA64_DIRECT_MAP_SIZE,
    }
}

pub(crate) fn la64_pt_node_ptr(phys: PhysAddr) -> *mut u8 {
    #[cfg(target_arch = "loongarch64")]
    {
        la64_cached_virt(phys.0) as *mut u8
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        phys.0 as *mut u8
    }
}

pub(crate) fn zero_la64_pt_node(phys: PhysAddr) {
    unsafe {
        core::ptr::write_bytes(
            la64_pt_node_ptr(phys),
            0,
            <Platform as PlatformConfig>::PAGE_SIZE,
        );
    }
}

pub(crate) fn la64_fixup_lookup(fault_pc: usize) -> Option<usize> {
    #[cfg(target_arch = "loongarch64")]
    {
        for entry in &LA64_FIXUP_TABLE {
            let start = entry.pc_start as usize;
            let end = entry.pc_end as usize;
            if fault_pc >= start && fault_pc < end {
                return Some(entry.recovery_pc as usize);
            }
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = fault_pc;
    None
}

pub(crate) fn align_down(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

pub(crate) fn la64_page_table_mut_from_phys(phys: PhysAddr) -> &'static mut [u64; 512] {
    unsafe { &mut *(la64_pt_node_ptr(phys) as *mut [u64; 512]) }
}

pub(crate) fn la64_kernel_addr_to_phys(addr: usize) -> usize {
    addr & LA64_PHYS_ADDR_MASK
}

pub(crate) fn alloc_la64_asid() -> Result<Asid, PmapError> {
    for (word_index, word) in LA64_ALLOCATED_ASIDS
        .iter()
        .enumerate()
        .take(LA64_ASID_BITMAP_WORDS)
    {
        loop {
            let allocated = word.load(Ordering::Acquire);
            // ASID 0 is reserved (word 0, bit 0).
            let reserved = if word_index == 0 { 1 } else { 0 };
            if allocated | reserved == u64::MAX {
                break;
            }
            for bit_index in 0..u64::BITS as usize {
                let asid = word_index * u64::BITS as usize + bit_index;
                if asid == 0 || asid >= LA64_ASID_CAPACITY {
                    continue;
                }
                let bit = 1u64 << bit_index;
                if allocated & bit != 0 {
                    continue;
                }
                if word
                    .compare_exchange(
                        allocated,
                        allocated | bit,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(Asid(asid as u16));
                }
                break;
            }
        }
    }
    Err(PmapError::Exhausted)
}

pub(crate) fn free_la64_asid(asid: Asid) {
    let asid = asid.0 as usize;
    if asid == 0 || asid >= LA64_ASID_CAPACITY {
        return;
    }
    let word_index = asid / u64::BITS as usize;
    let bit_index = asid % u64::BITS as usize;
    LA64_ALLOCATED_ASIDS[word_index].fetch_and(!(1u64 << bit_index), Ordering::AcqRel);
}

#[cfg(feature = "la64-boot-trace")]
fn trace_pmap_literal(bytes: &[u8]) {
    console_write_literal(bytes);
}

#[cfg(not(feature = "la64-boot-trace"))]
fn trace_pmap_literal(_bytes: &[u8]) {}

#[cfg(feature = "la64-boot-trace")]
fn trace_pmap_hex(value: usize) {
    console_write_hex(value);
}

#[cfg(not(feature = "la64-boot-trace"))]
fn trace_pmap_hex(_value: usize) {}

pub(crate) struct La64PmapSwitch {
    pub(crate) asid: usize,
    pub(crate) pgdl: usize,
    pub(crate) pgdh: usize,
    pub(crate) switch_required: bool,
}

pub(crate) fn prepare_la64_pmap_switch(root: &PmapRoot) -> Result<La64PmapSwitch, PmapError> {
    let pgdh = ensure_la64_kernel_pgdh_bootstrap_mapped()?;
    let asid = root.asid().0 as usize & LA64_ASID_MASK;
    let pgdl = root.phys().0;
    let cpu = la64_current_cpu_id().0;
    if cpu >= LA64_MAX_BOOT_CPUS {
        return Err(PmapError::InvalidRequest);
    }
    let switch_required = LA64_ACTIVE_ASID[cpu].load(Ordering::Acquire) != asid
        || LA64_ACTIVE_PGDL[cpu].load(Ordering::Acquire) != pgdl
        || LA64_ACTIVE_PGDH[cpu].load(Ordering::Acquire) != pgdh.0;

    if switch_required {
        configure_la64_page_walk_csrs();
    }

    Ok(La64PmapSwitch {
        asid,
        pgdl,
        pgdh: pgdh.0,
        switch_required,
    })
}

fn begin_la64_pmap_transition(cpu: usize, asid: usize, pgdl: usize) {
    let sequence = LA64_PMAP_SWITCH_SEQ[cpu].load(Ordering::Acquire);
    assert_eq!(sequence & 1, 0, "nested LA64 pmap transition on cpu {cpu}");
    LA64_PMAP_SWITCH_SEQ[cpu]
        .compare_exchange(
            sequence,
            sequence.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .expect("concurrent LA64 pmap transition on one hart");

    // Once the sequence is odd, root teardown treats this hart as
    // non-quiescent even before it can observe which root is incoming. This
    // closes the tuple -> residency publication gap.
    LA64_SWITCHING_PGDL[cpu].store(pgdl, Ordering::Relaxed);
    LA64_SWITCHING_ASID[cpu].store(asid, Ordering::Release);
    if asid != 0 && asid < LA64_ASID_RESIDENCY.len() {
        LA64_ASID_RESIDENCY[asid].fetch_or(1u64 << cpu, Ordering::AcqRel);
    }
}

fn finish_la64_pmap_transition(cpu: usize) {
    LA64_SWITCHING_ASID[cpu].store(0, Ordering::Relaxed);
    LA64_SWITCHING_PGDL[cpu].store(0, Ordering::Relaxed);
    let sequence = LA64_PMAP_SWITCH_SEQ[cpu].fetch_add(1, Ordering::Release);
    assert_eq!(
        sequence & 1,
        1,
        "LA64 pmap transition completed without begin on cpu {cpu}"
    );
}

/// Publish a transition before hardware can start using the incoming PGDL.
///
/// The odd sequence is the lifetime barrier: teardown cannot declare any root
/// quiescent while a hart is between this function and
/// [`tx_la64_finish_pmap_switch`]. The incoming residency is then installed
/// before the CSR writes, while the outgoing residency remains until finish.
#[unsafe(no_mangle)]
pub extern "C" fn tx_la64_begin_pmap_switch(asid: usize, pgdl: usize) {
    let cpu = la64_current_cpu_id().0;
    if cpu >= LA64_MAX_BOOT_CPUS || cpu >= u64::BITS as usize {
        return;
    }
    begin_la64_pmap_transition(cpu, asid, pgdl);
}

/// Complete an LA64 address-space switch after ASID/PGDL/PGDH and INVTLB are
/// installed in hardware.
///
/// This is called directly by the no-return userspace-entry assembly. Clearing
/// the outgoing residency any earlier would let another hart free that root
/// while this hart could still walk it.
#[unsafe(no_mangle)]
pub extern "C" fn tx_la64_finish_pmap_switch(asid: usize, pgdl: usize, pgdh: usize) {
    let cpu = la64_current_cpu_id().0;
    if cpu >= LA64_MAX_BOOT_CPUS || cpu >= u64::BITS as usize {
        return;
    }
    let previous = LA64_ACTIVE_ASID[cpu].load(Ordering::Relaxed);
    LA64_ACTIVE_PGDL[cpu].store(pgdl, Ordering::Relaxed);
    LA64_ACTIVE_PGDH[cpu].store(pgdh, Ordering::Relaxed);
    LA64_ACTIVE_ASID[cpu].store(asid, Ordering::Relaxed);
    if previous != 0 && previous != asid && previous < LA64_ASID_RESIDENCY.len() {
        LA64_ASID_RESIDENCY[previous].fetch_and(!(1u64 << cpu), Ordering::AcqRel);
    }
    if asid != 0 && asid < LA64_ASID_RESIDENCY.len() {
        // Keep this idempotent for the standalone activation path and host
        // tests; begin normally published the bit before the CSR writes.
        LA64_ASID_RESIDENCY[asid].fetch_or(1u64 << cpu, Ordering::AcqRel);
    }
    finish_la64_pmap_transition(cpu);
}

pub(crate) fn stable_la64_active_root(cpu: usize) -> Option<(usize, usize, usize)> {
    let before = LA64_PMAP_SWITCH_SEQ[cpu].load(Ordering::Acquire);
    if before & 1 != 0 {
        return None;
    }
    let asid = LA64_ACTIVE_ASID[cpu].load(Ordering::Relaxed);
    let pgdl = LA64_ACTIVE_PGDL[cpu].load(Ordering::Relaxed);
    let pgdh = LA64_ACTIVE_PGDH[cpu].load(Ordering::Relaxed);
    let after = LA64_PMAP_SWITCH_SEQ[cpu].load(Ordering::Acquire);
    if before == after && after & 1 == 0 {
        Some((asid, pgdl, pgdh))
    } else {
        None
    }
}

fn deactivate_la64_user_pmap_matching(expected: Option<(usize, usize)>) -> bool {
    let cpu = la64_current_cpu_id().0;
    if cpu >= LA64_MAX_BOOT_CPUS || cpu >= u64::BITS as usize {
        return false;
    }
    let previous_crmd = read_la64_csr(LA64_CSR_CRMD);
    write_la64_csr(LA64_CSR_CRMD, previous_crmd & !LA64_CRMD_IE);

    let Some((asid, active_pgdl, _)) = stable_la64_active_root(cpu) else {
        panic!("LA64 current hart is already switching pmaps");
    };
    if asid == 0 {
        if previous_crmd & LA64_CRMD_IE != 0 {
            let crmd = read_la64_csr(LA64_CSR_CRMD);
            write_la64_csr(LA64_CSR_CRMD, crmd | LA64_CRMD_IE);
        }
        return false;
    }
    if expected.is_some_and(|(expected_asid, expected_pgdl)| {
        asid != expected_asid || active_pgdl != expected_pgdl
    }) {
        if previous_crmd & LA64_CRMD_IE != 0 {
            let crmd = read_la64_csr(LA64_CSR_CRMD);
            write_la64_csr(LA64_CSR_CRMD, crmd | LA64_CRMD_IE);
        }
        return false;
    }
    let pgdh = LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire);
    assert_ne!(
        pgdh, 0,
        "LA64 kernel PGDH must exist before leaving userspace"
    );

    begin_la64_pmap_transition(cpu, 0, 0);
    write_la64_csr(LA64_CSR_ASID, 0);
    write_la64_csr(LA64_CSR_PGDL, 0);
    write_la64_csr(LA64_CSR_PGDH, pgdh);
    let crmd = LA64_CRMD_PG | LA64_CRMD_DATF_CC | LA64_CRMD_DATM_CC;
    write_la64_csr(LA64_CSR_CRMD, crmd);
    la64_invtlb_all();

    LA64_ACTIVE_PGDL[cpu].store(0, Ordering::Relaxed);
    LA64_ACTIVE_PGDH[cpu].store(pgdh, Ordering::Relaxed);
    LA64_ACTIVE_ASID[cpu].store(0, Ordering::Relaxed);
    if asid < LA64_ASID_RESIDENCY.len() {
        LA64_ASID_RESIDENCY[asid].fetch_and(!(1u64 << cpu), Ordering::AcqRel);
    }
    finish_la64_pmap_transition(cpu);

    if previous_crmd & LA64_CRMD_IE != 0 {
        let crmd = read_la64_csr(LA64_CSR_CRMD);
        write_la64_csr(LA64_CSR_CRMD, crmd | LA64_CRMD_IE);
    }
    true
}

/// Switch this hart from whichever user PGDL it currently owns to the
/// permanent kernel PGDH, and publish departure only after hardware is safe.
pub(crate) fn deactivate_la64_user_pmap() {
    let _ = deactivate_la64_user_pmap_matching(None);
}

/// Deactivate only when the current hart is using the root being destroyed.
///
/// Destroying root A must never switch an unrelated active root B away from
/// the CPU.
pub(crate) fn deactivate_la64_user_pmap_if_matches(asid: Asid, pgdl: PhysAddr) {
    let _ = deactivate_la64_user_pmap_matching(Some((asid.0 as usize, pgdl.0)));
}

pub(crate) fn la64_asid_residency_mask(asid: Asid) -> CpuMask {
    let index = asid.0 as usize;
    if index >= LA64_ASID_RESIDENCY.len() {
        return CpuMask::EMPTY;
    }
    CpuMask::from_bits(LA64_ASID_RESIDENCY[index].load(Ordering::Acquire))
}

pub(crate) fn wait_for_la64_asid_quiescence(asid: Asid, pgdl: PhysAddr) {
    let target_asid = asid.0 as usize;
    let started = super::la64_percpu::la64_read_stable_counter();
    let diagnostic_after = la64_timebase_frequency_hz().max(1);
    let mut diagnostic_emitted = false;
    let mut wait = TlbProgressSpinWait::new();
    loop {
        let mut transition_or_owner = false;
        for cpu in 0..LA64_MAX_BOOT_CPUS {
            let Some((active_asid, active_pgdl, _)) = stable_la64_active_root(cpu) else {
                // An odd sequence starts before the incoming root is
                // published. Conservatively block every root teardown across
                // this very short hardware-switch interval.
                transition_or_owner = true;
                break;
            };
            if active_asid == target_asid && active_pgdl == pgdl.0 {
                transition_or_owner = true;
                break;
            }
        }
        if !transition_or_owner && la64_asid_residency_mask(asid).is_empty() {
            break;
        }
        if !diagnostic_emitted
            && super::la64_percpu::la64_read_stable_counter().wrapping_sub(started)
                >= diagnostic_after
        {
            diagnostic_emitted = true;
            console_write_literal(b"txkernel:la64-pmap-quiesce-stall:target-asid=0x");
            console_write_hex(target_asid);
            console_write_literal(b":target-pgdl=0x");
            console_write_hex(pgdl.0);
            console_write_literal(b":residency=0x");
            console_write_hex(la64_asid_residency_mask(asid).bits() as usize);
            console_write_literal(b"\n");
            for cpu in 0..LA64_MAX_BOOT_CPUS {
                let sequence = LA64_PMAP_SWITCH_SEQ[cpu].load(Ordering::Acquire);
                let active_asid = LA64_ACTIVE_ASID[cpu].load(Ordering::Relaxed);
                let active_pgdl = LA64_ACTIVE_PGDL[cpu].load(Ordering::Relaxed);
                let switching_asid = LA64_SWITCHING_ASID[cpu].load(Ordering::Relaxed);
                let switching_pgdl = LA64_SWITCHING_PGDL[cpu].load(Ordering::Relaxed);
                if sequence != 0
                    || active_asid != 0
                    || active_pgdl != 0
                    || switching_asid != 0
                    || switching_pgdl != 0
                {
                    console_write_literal(b"txkernel:la64-pmap-quiesce-stall:cpu=0x");
                    console_write_hex(cpu);
                    console_write_literal(b":seq=0x");
                    console_write_hex(sequence as usize);
                    console_write_literal(b":active-asid=0x");
                    console_write_hex(active_asid);
                    console_write_literal(b":active-pgdl=0x");
                    console_write_hex(active_pgdl);
                    console_write_literal(b":switching-asid=0x");
                    console_write_hex(switching_asid);
                    console_write_literal(b":switching-pgdl=0x");
                    console_write_hex(switching_pgdl);
                    console_write_literal(b"\n");
                }
            }
        }
        spin_with_la64_tlb_progress(&mut wait);
    }
}

/// Flush every online hart after residency reaches zero and before the
/// numeric ASID or any page-table page can be reused.
pub(crate) fn invalidate_la64_asid_before_reuse() {
    la64_invtlb_all();
    let targets = CpuMask::from_bits(
        <Platform as SmpIf>::online_cpus().bits() & !CpuMask::single(la64_current_cpu_id()).bits(),
    );
    la64_remote_tlb_shootdown(targets);
}

#[inline]
pub(crate) fn la64_tlb_generation_reached(completed: u64, requested: u64) -> bool {
    (completed.wrapping_sub(requested) as i64) >= 0
}

pub(crate) fn try_pin_la64_tlb_target(cpu: usize) -> bool {
    if cpu >= LA64_MAX_BOOT_CPUS {
        return false;
    }
    let bit = 1u64 << cpu;
    if LA64_TLB_ACCEPTING_CPUS.load(Ordering::Acquire) & bit == 0 {
        return false;
    }
    LA64_TLB_TARGET_USERS[cpu].fetch_add(1, Ordering::AcqRel);
    if LA64_TLB_ACCEPTING_CPUS.load(Ordering::Acquire) & bit != 0 {
        return true;
    }
    LA64_TLB_TARGET_USERS[cpu].fetch_sub(1, Ordering::AcqRel);
    false
}

pub(crate) fn unpin_la64_tlb_target(cpu: usize) {
    let previous = LA64_TLB_TARGET_USERS[cpu].fetch_sub(1, Ordering::AcqRel);
    assert_ne!(previous, 0, "unbalanced LA64 TLB target pin");
}

pub(crate) fn mark_la64_tlb_cpu_online(cpu: CpuId) {
    if cpu.0 < LA64_MAX_BOOT_CPUS {
        debug_assert_eq!(LA64_TLB_TARGET_USERS[cpu.0].load(Ordering::Acquire), 0);
        LA64_TLB_ACCEPTING_CPUS.fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
    }
}

/// Stop accepting new synchronous requests, then drain every sender which
/// acquired this hart before the transition.
pub(crate) fn prepare_la64_tlb_cpu_offline() {
    let cpu = la64_current_cpu_id();
    if cpu.0 >= LA64_MAX_BOOT_CPUS {
        return;
    }

    // A permanently parked CPU must not retain a root-lifetime reference.
    deactivate_la64_user_pmap();
    let bit = CpuMask::single(cpu).bits();
    LA64_ONLINE_CPUS.fetch_and(!bit, Ordering::AcqRel);
    LA64_TLB_ACCEPTING_CPUS.fetch_and(!bit, Ordering::AcqRel);

    // A sender which pinned before the accepting-bit clear may publish its
    // generation afterwards. The pin remains held until completion, so
    // servicing while users are non-zero closes that race.
    let mut wait = TlbProgressSpinWait::new();
    while LA64_TLB_TARGET_USERS[cpu.0].load(Ordering::Acquire) != 0 {
        spin_with_la64_tlb_progress(&mut wait);
    }
    service_la64_pending_tlb_shootdown();
}

/// Service the current hart's full-TLB mailbox without depending on CRMD.IE.
///
/// This function deliberately acquires no VM, heap, or IPI locks. It is safe
/// both from the ordinary IPI trap and from the pmap/vmalloc lock contention
/// loops that break a sender-holds-lock / target-waits-lock cycle.
///
/// The return value reports whether this call serviced, or joined an already
/// active service of, a pending mailbox generation. A later hardware IPI for
/// a generation that was already completed by a polling safe point may
/// conservatively perform one redundant full flush.
pub(crate) fn service_la64_pending_tlb_shootdown() -> bool {
    let cpu = la64_current_cpu_id().0;
    if cpu >= LA64_MAX_BOOT_CPUS {
        return false;
    }

    let requested = LA64_TLB_SHOOTDOWN_REQUESTED[cpu].load(Ordering::Acquire);
    let completed = LA64_TLB_SHOOTDOWN_COMPLETED[cpu].load(Ordering::Acquire);
    if requested == completed {
        return false;
    }

    // A nested hardware IPI may arrive while a polling safe point is already
    // servicing the mailbox. The first context owns INVTLB/completion; the
    // nested context only needs to clear its hardware interrupt on return.
    if LA64_TLB_SHOOTDOWN_SERVICING[cpu]
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return true;
    }

    'service: loop {
        loop {
            let requested = LA64_TLB_SHOOTDOWN_REQUESTED[cpu].load(Ordering::Acquire);
            let completed = LA64_TLB_SHOOTDOWN_COMPLETED[cpu].load(Ordering::Relaxed);
            if requested == completed {
                break;
            }
            la64_invtlb_all();
            LA64_TLB_SHOOTDOWN_COMPLETED[cpu].store(requested, Ordering::Release);
        }

        // Release ownership before the final recheck. A request can arrive
        // after the inner loop observed equality while its hardware IPI is
        // concurrently being acknowledged by a nested context. Rechecking
        // after this release either lets us reacquire and drain that request,
        // observes another context doing so, or leaves a later request paired
        // with its still-pending hardware IPI.
        LA64_TLB_SHOOTDOWN_SERVICING[cpu].store(false, Ordering::Release);
        let requested = LA64_TLB_SHOOTDOWN_REQUESTED[cpu].load(Ordering::Acquire);
        let completed = LA64_TLB_SHOOTDOWN_COMPLETED[cpu].load(Ordering::Acquire);
        if requested == completed {
            break 'service;
        }
        if LA64_TLB_SHOOTDOWN_SERVICING[cpu]
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            break 'service;
        }
    }
    true
}

/// Force remote harts through a full INVTLB and wait for their generations.
///
/// A per-target generation replaces the old global shootdown lock and shared
/// acknowledgement bit. Concurrent senders can coalesce into one remote full
/// flush, and every sender services its own inbound mailbox while waiting so
/// simultaneous shootdowns cannot deadlock each other with interrupts masked.
pub(crate) fn la64_remote_tlb_shootdown(targets: CpuMask) {
    let current = la64_current_cpu_id();
    let targets = CpuMask::from_bits(targets.bits() & !CpuMask::single(current).bits());
    if targets.is_empty() {
        return;
    }

    let mut generations = [0u64; LA64_MAX_BOOT_CPUS];
    let mut pinned = 0u64;
    let mut candidates = targets.bits();
    while candidates != 0 {
        let cpu = candidates.trailing_zeros() as usize;
        let bit = 1u64 << cpu;
        if !try_pin_la64_tlb_target(cpu) {
            candidates &= candidates - 1;
            continue;
        }
        pinned |= bit;
        let generation = LA64_TLB_SHOOTDOWN_REQUESTED[cpu]
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        generations[cpu] = generation;
        <Platform as SmpIf>::send_ipi(CpuId(cpu), IpiKind::TlbShootdown);
        candidates &= candidates - 1;
    }

    let started = super::la64_percpu::la64_read_stable_counter();
    let diagnostic_after = la64_timebase_frequency_hz().max(1);
    let mut pending = pinned;
    let mut wait = TlbProgressSpinWait::new();
    while pending != 0 {
        let mut remaining = pending;
        while remaining != 0 {
            let cpu = remaining.trailing_zeros() as usize;
            let bit = 1u64 << cpu;
            let completed = LA64_TLB_SHOOTDOWN_COMPLETED[cpu].load(Ordering::Acquire);
            if la64_tlb_generation_reached(completed, generations[cpu]) {
                pending &= !bit;
                unpin_la64_tlb_target(cpu);
            }
            remaining &= remaining - 1;
        }
        if super::la64_percpu::la64_read_stable_counter().wrapping_sub(started) >= diagnostic_after
            && LA64_TLB_STALL_DIAG_EMITTED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            console_write_literal(b"txkernel:la64-tlb-shootdown-stall:sender=0x");
            console_write_hex(current.0);
            console_write_literal(b":pending=0x");
            console_write_hex(pending as usize);
            console_write_literal(b":online=0x");
            console_write_hex(<Platform as SmpIf>::online_cpus().bits() as usize);
            console_write_literal(b":accepting=0x");
            console_write_hex(LA64_TLB_ACCEPTING_CPUS.load(Ordering::Acquire) as usize);
            console_write_literal(b"\n");
            let mut stalled = pending;
            while stalled != 0 {
                let cpu = stalled.trailing_zeros() as usize;
                let bit = 1u64 << cpu;
                console_write_literal(b"txkernel:la64-tlb-shootdown-stall:target=0x");
                console_write_hex(cpu);
                console_write_literal(b":requested=0x");
                console_write_hex(
                    LA64_TLB_SHOOTDOWN_REQUESTED[cpu].load(Ordering::Acquire) as usize
                );
                console_write_literal(b":completed=0x");
                console_write_hex(
                    LA64_TLB_SHOOTDOWN_COMPLETED[cpu].load(Ordering::Acquire) as usize
                );
                console_write_literal(b":servicing=0x");
                console_write_hex(
                    LA64_TLB_SHOOTDOWN_SERVICING[cpu].load(Ordering::Acquire) as usize
                );
                console_write_literal(b":users=0x");
                console_write_hex(LA64_TLB_TARGET_USERS[cpu].load(Ordering::Acquire));
                console_write_literal(b"\n");
                stalled &= !bit;
            }
        }
        spin_with_la64_tlb_progress(&mut wait);
    }
}

pub(crate) fn activate_la64_pmap(root: &PmapRoot) -> Result<(), PmapError> {
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:activate:start\n");
    let switch = prepare_la64_pmap_switch(root)?;
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:pgdh=0x");
    trace_pmap_hex(switch.pgdh);
    trace_pmap_literal(b"\n");
    if !switch.switch_required {
        trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:switch:skip\n");
        return Ok(());
    }
    let previous_crmd = read_la64_csr(LA64_CSR_CRMD);
    write_la64_csr(LA64_CSR_CRMD, previous_crmd & !LA64_CRMD_IE);
    tx_la64_begin_pmap_switch(switch.asid, switch.pgdl);
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:pwcl:ok\n");

    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:root=0x");
    trace_pmap_hex(switch.pgdl);
    trace_pmap_literal(b":asid=0x");
    trace_pmap_hex(switch.asid);
    trace_pmap_literal(b"\n");
    write_la64_csr(LA64_CSR_ASID, switch.asid);
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:asid:ok\n");
    write_la64_csr(LA64_CSR_PGDL, switch.pgdl);
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:pgdl:ok\n");
    write_la64_csr(LA64_CSR_PGDH, switch.pgdh);
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:pgdh-write:ok\n");

    let crmd = LA64_CRMD_PG | LA64_CRMD_DATF_CC | LA64_CRMD_DATM_CC;
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:crmd-new=0x00000000000000b0\n");
    write_la64_csr(LA64_CSR_CRMD, crmd);
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:crmd:ok\n");
    la64_invtlb_all();
    trace_pmap_literal(b"txkernel:qemu-loongarch64-virt:pmap:invtlb:ok\n");

    tx_la64_finish_pmap_switch(switch.asid, switch.pgdl, switch.pgdh);
    if previous_crmd & LA64_CRMD_IE != 0 {
        let crmd = read_la64_csr(LA64_CSR_CRMD);
        write_la64_csr(LA64_CSR_CRMD, crmd | LA64_CRMD_IE);
    }
    Ok(())
}

pub(crate) fn ensure_la64_kernel_pgdh_bootstrap_mapped() -> Result<PhysAddr, PmapError> {
    let root = ensure_la64_kernel_pgdh_root()?;
    let mut wait = TlbProgressSpinWait::new();
    loop {
        match LA64_KERNEL_PGDH_BOOTSTRAP_STATE.load(Ordering::Acquire) {
            LA64_PGDH_BOOTSTRAP_READY => return Ok(root),
            LA64_PGDH_BOOTSTRAP_BUILDING => {
                // The builder can be waiting on a pmap operation whose
                // shootdown targets us, so a plain spin is not a valid SMP
                // once primitive on LA64.
                spin_with_la64_tlb_progress(&mut wait);
            }
            LA64_PGDH_BOOTSTRAP_UNINIT => {
                if LA64_KERNEL_PGDH_BOOTSTRAP_STATE
                    .compare_exchange(
                        LA64_PGDH_BOOTSTRAP_UNINIT,
                        LA64_PGDH_BOOTSTRAP_BUILDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    spin_with_la64_tlb_progress(&mut wait);
                    continue;
                }

                let result = (|| {
                    let image = linked_kernel_image();
                    let start = align_down(image.start.0, <Platform as PlatformConfig>::PAGE_SIZE);
                    let end = align_up(image.end().0, <Platform as PlatformConfig>::PAGE_SIZE);
                    let mut phys = start;
                    while phys < end {
                        let virt = VirtAddr(la64_cached_virt(phys));
                        let reservation = reserve_la64_mapping_in_root(
                            root,
                            virt,
                            PhysAddr(phys),
                            PmapReserveKind::Page4K,
                        )?;
                        if let Some(reservation) = reservation {
                            register_la64_committed_intermediates(reservation.intermediates());
                            let permissions = PmapPermissions::KERNEL_RW
                                .union(PmapPermissions::EXECUTE)
                                .union(PmapPermissions::GLOBAL);
                            let leaf = encode_la64_leaf_pte(PhysAddr(phys), permissions);
                            write_la64_leaf(root, virt, PmapReserveKind::Page4K, leaf)?;
                        }
                        phys = phys.saturating_add(<Platform as PlatformConfig>::PAGE_SIZE);
                    }
                    Ok(root)
                })();

                LA64_KERNEL_PGDH_BOOTSTRAP_STATE.store(
                    if result.is_ok() {
                        LA64_PGDH_BOOTSTRAP_READY
                    } else {
                        // Existing leaves and committed intermediates remain
                        // valid. A later builder may resume the idempotent walk.
                        LA64_PGDH_BOOTSTRAP_UNINIT
                    },
                    Ordering::Release,
                );
                return result;
            }
            state => panic!("invalid LA64 PGDH bootstrap state {state}"),
        }
    }
}

pub(crate) fn ensure_la64_kernel_pgdh_root() -> Result<PhysAddr, PmapError> {
    // This is the steady-state high-half kernel page-table root. LA64 early
    // boot reaches `CoreInit` through DMW, so this root is deliberately not the
    // `BootstrapPmapInfo::root` published by `boot_facts`.
    let existing = LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(PhysAddr(existing));
    }

    let node = Platform::alloc_pt_node().map_err(|_| PmapError::Exhausted)?;
    zero_la64_pt_node(node.phys);
    match LA64_KERNEL_PGDH_PHYS.compare_exchange(
        0,
        node.phys.0,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(node.phys),
        Err(existing) => {
            Platform::free_pt_node(node);
            Ok(PhysAddr(existing))
        }
    }
}

pub(crate) fn configure_la64_page_walk_csrs() {
    write_la64_csr(LA64_CSR_PWCL, la64_pwcl_value());
    write_la64_csr(LA64_CSR_PWCH, la64_pwch_value());
    write_la64_csr(LA64_CSR_STLBPS, <Platform as PlatformConfig>::PAGE_SHIFT);
    write_la64_csr(LA64_CSR_TLBREHI, <Platform as PlatformConfig>::PAGE_SHIFT);
}

pub(crate) const fn la64_pwcl_value() -> usize {
    (12usize) | (9usize << 5) | (21usize << 10) | (9usize << 15) | (30usize << 20) | (9usize << 25)
}

pub(crate) const fn la64_pwch_value() -> usize {
    39usize | (9usize << 6)
}

pub(crate) fn reserve_la64_kernel_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    validate_la64_kernel_mapping(virt, phys, kind)?;
    let root = ensure_la64_kernel_pgdh_root()?;
    reserve_la64_mapping_in_root(root, virt, phys, kind)
}

pub(crate) fn rollback_la64_kernel_mapping(reservation: PmapReservation) {
    let root = LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire);
    if root != 0 {
        rollback_la64_intermediates(
            PhysAddr(root),
            reservation.virt(),
            reservation.intermediates(),
        );
    }
    la64_invtlb_global(reservation.virt());
}

pub(crate) fn commit_la64_kernel_mapping(
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    let virt = reservation.virt();
    publish_la64_kernel_mapping(reservation, permissions);
    la64_invtlb_global(virt);
}

/// Publish a reservation for a previously empty leaf. New vmap mappings have
/// no stale valid translation, so population does not require INVTLB.
pub(crate) fn commit_new_la64_kernel_mapping(
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    publish_la64_kernel_mapping(reservation, permissions);
}

fn publish_la64_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
    let root = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    assert_ne!(root.0, 0, "LA64 kernel PGDH root must exist");
    register_la64_committed_intermediates(reservation.intermediates());
    let leaf = encode_la64_leaf_pte(reservation.phys(), permissions);
    write_la64_leaf(root, reservation.virt(), reservation.kind(), leaf)
        .expect("reserved LA64 kernel leaf slot");
}

pub(crate) fn unmap_la64_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_la64_kernel_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let root = LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire);
    if root == 0 {
        return Ok(None);
    }
    // Committed kernel intermediate tables remain resident.  The vmalloc
    // window is bounded, and reclaiming a now-empty branch here would happen
    // before the caller's cross-hart shootdown.  A remote page-table walk
    // could therefore dereference a page that had already been returned to
    // the allocator and reused for unrelated data.
    unmap_la64_mapping_in_root(PhysAddr(root), virt, kind)
}

pub(crate) fn protect_la64_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    validate_la64_kernel_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    validate_la64_leaf_permissions(permissions, false)?;
    let root = LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire);
    if root == 0 {
        return Ok(None);
    }
    protect_la64_mapping_in_root(PhysAddr(root), virt, kind, permissions)
}

pub(crate) struct La64EnsuredTable {
    table: &'static mut [u64; 512],
    node: Option<PtNode>,
}

pub(crate) fn reserve_la64_mapping_in_root(
    root: PhysAddr,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    let root_table = la64_page_table_mut_from_phys(root);
    let ensured_l2 = ensure_la64_child_table(&mut root_table[la64_l3_index(virt.0)])?;
    let l2_node = ensured_l2.node;

    match kind {
        PmapReserveKind::Superpage1G => {
            let intermediates = PmapReservationIntermediates {
                l2: l2_node,
                l1: None,
                l0: None,
            };
            reserve_la64_leaf_slot(
                root,
                virt,
                phys,
                kind,
                ensured_l2.table[la64_l2_index(virt.0)],
                intermediates,
            )
        }
        PmapReserveKind::Superpage2M => {
            let ensured_l1 =
                match ensure_la64_child_table(&mut ensured_l2.table[la64_l2_index(virt.0)]) {
                    Ok(table) => table,
                    Err(error) => {
                        rollback_la64_intermediates(
                            root,
                            virt,
                            PmapReservationIntermediates {
                                l2: l2_node,
                                l1: None,
                                l0: None,
                            },
                        );
                        return Err(error);
                    }
                };
            let intermediates = PmapReservationIntermediates {
                l2: l2_node,
                l1: ensured_l1.node,
                l0: None,
            };
            reserve_la64_leaf_slot(
                root,
                virt,
                phys,
                kind,
                ensured_l1.table[la64_l1_index(virt.0)],
                intermediates,
            )
        }
        PmapReserveKind::Page4K => {
            let ensured_l1 =
                match ensure_la64_child_table(&mut ensured_l2.table[la64_l2_index(virt.0)]) {
                    Ok(table) => table,
                    Err(error) => {
                        rollback_la64_intermediates(
                            root,
                            virt,
                            PmapReservationIntermediates {
                                l2: l2_node,
                                l1: None,
                                l0: None,
                            },
                        );
                        return Err(error);
                    }
                };
            let l1_node = ensured_l1.node;
            let ensured_l0 =
                match ensure_la64_child_table(&mut ensured_l1.table[la64_l1_index(virt.0)]) {
                    Ok(table) => table,
                    Err(error) => {
                        rollback_la64_intermediates(
                            root,
                            virt,
                            PmapReservationIntermediates {
                                l2: l2_node,
                                l1: l1_node,
                                l0: None,
                            },
                        );
                        return Err(error);
                    }
                };
            let intermediates = PmapReservationIntermediates {
                l2: l2_node,
                l1: l1_node,
                l0: ensured_l0.node,
            };
            reserve_la64_leaf_slot(
                root,
                virt,
                phys,
                kind,
                ensured_l0.table[la64_l0_index(virt.0)],
                intermediates,
            )
        }
    }
}

pub(crate) fn reserve_la64_leaf_slot(
    root: PhysAddr,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
    current: u64,
    intermediates: PmapReservationIntermediates,
) -> Result<Option<PmapReservation>, PmapError> {
    if current == 0 {
        return Ok(Some(PmapReservation::new_with_intermediates(
            virt,
            phys,
            kind,
            intermediates,
        )));
    }

    let result = if la64_pte_is_leaf(current) && la64_pte_phys(current) == phys {
        Ok(None)
    } else {
        Err(PmapError::AlreadyMapped)
    };
    rollback_la64_intermediates(root, virt, intermediates);
    result
}

pub(crate) fn reserve_la64_user_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    if kind != PmapReserveKind::Page4K {
        return Err(PmapError::Unsupported);
    }
    validate_la64_user_mapping(virt, phys, kind)?;

    let root_table = la64_page_table_mut_from_phys(root.phys());
    let ensured_l2 = ensure_la64_child_table(&mut root_table[la64_l3_index(virt.0)])?;
    let l2_node = ensured_l2.node;
    let ensured_l1 = match ensure_la64_child_table(&mut ensured_l2.table[la64_l2_index(virt.0)]) {
        Ok(table) => table,
        Err(error) => {
            rollback_la64_intermediates(
                root.phys(),
                virt,
                PmapReservationIntermediates {
                    l2: l2_node,
                    l1: None,
                    l0: None,
                },
            );
            return Err(error);
        }
    };
    let l1_node = ensured_l1.node;
    let ensured_l0 = match ensure_la64_child_table(&mut ensured_l1.table[la64_l1_index(virt.0)]) {
        Ok(table) => table,
        Err(error) => {
            rollback_la64_intermediates(
                root.phys(),
                virt,
                PmapReservationIntermediates {
                    l2: l2_node,
                    l1: l1_node,
                    l0: None,
                },
            );
            return Err(error);
        }
    };
    let intermediates = PmapReservationIntermediates {
        l2: l2_node,
        l1: l1_node,
        l0: ensured_l0.node,
    };
    let current = ensured_l0.table[la64_l0_index(virt.0)];
    if current != 0 {
        rollback_la64_intermediates(root.phys(), virt, intermediates);
        return Err(PmapError::AlreadyMapped);
    }

    Ok(Some(PmapReservation::new_with_intermediates(
        virt,
        phys,
        kind,
        intermediates,
    )))
}

pub(crate) fn rollback_la64_user_mapping(root: &PmapRoot, reservation: PmapReservation) {
    rollback_la64_intermediates(root.phys(), reservation.virt(), reservation.intermediates());
    la64_invtlb_global(reservation.virt());
}

pub(crate) fn commit_la64_user_mapping(
    root: &PmapRoot,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    assert_eq!(reservation.kind(), PmapReserveKind::Page4K);
    validate_la64_leaf_permissions(permissions, true).expect("invalid LA64 user permissions");
    register_la64_committed_intermediates(reservation.intermediates());
    let leaf = encode_la64_leaf_pte(reservation.phys(), permissions);
    let l0 = la64_l0_table_mut(root.phys(), reservation.virt()).expect("reserved LA64 L0 table");
    l0[la64_l0_index(reservation.virt().0)] = leaf;
    la64_invtlb_asid(root.asid(), reservation.virt());
}

pub(crate) fn unmap_la64_user_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    if kind != PmapReserveKind::Page4K {
        return Err(PmapError::Unsupported);
    }
    validate_la64_user_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let Some(l0) = la64_l0_table_mut(root.phys(), virt) else {
        return Ok(None);
    };
    let slot = &mut l0[la64_l0_index(virt.0)];
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !la64_pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = la64_pte_phys(current);
    *slot = 0;
    // Keep committed L0/L1/L2 tables owned by this root until
    // destroy_pmap_root() has observed ASID quiescence.  Ordinary unmap only
    // invalidates the leaf; its shootdown has not completed yet, so pruning
    // and freeing an intermediate node here would race stale hardware walks.
    Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
}

pub(crate) fn protect_la64_user_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    if kind != PmapReserveKind::Page4K {
        return Err(PmapError::Unsupported);
    }
    validate_la64_user_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    validate_la64_leaf_permissions(permissions, true)?;
    let Some(l0) = la64_l0_table_mut(root.phys(), virt) else {
        return Ok(None);
    };
    let slot = &mut l0[la64_l0_index(virt.0)];
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !la64_pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let updated = encode_la64_leaf_pte(la64_pte_phys(current), permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, kind.size())))
}

pub(crate) fn ensure_la64_child_table(slot: &mut u64) -> Result<La64EnsuredTable, PmapError> {
    if *slot != 0 {
        if la64_pte_is_branch(*slot) {
            return Ok(La64EnsuredTable {
                table: la64_page_table_mut_from_phys(la64_pte_phys(*slot)),
                node: None,
            });
        }
        return Err(PmapError::AlreadyMapped);
    }

    let node = Platform::alloc_pt_node().map_err(|_| PmapError::Exhausted)?;
    zero_la64_pt_node(node.phys);
    *slot = encode_la64_branch_pte(node.phys);
    Ok(La64EnsuredTable {
        table: la64_page_table_mut_from_phys(node.phys),
        node: Some(node),
    })
}

pub(crate) fn rollback_la64_intermediates(
    root: PhysAddr,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = la64_l1_table_mut_from_root(root, virt) {
            let slot = &mut l1[la64_l1_index(virt.0)];
            if la64_pte_is_branch(*slot) && la64_pte_phys(*slot) == l0.phys {
                *slot = 0;
            }
        }
        Platform::free_pt_node(l0);
    }

    if let Some(l1) = intermediates.l1 {
        if let Some(l2) = la64_l2_table_mut_from_root(root, virt) {
            let slot = &mut l2[la64_l2_index(virt.0)];
            if la64_pte_is_branch(*slot) && la64_pte_phys(*slot) == l1.phys {
                *slot = 0;
            }
        }
        Platform::free_pt_node(l1);
    }

    if let Some(l2) = intermediates.l2 {
        let root_table = la64_page_table_mut_from_phys(root);
        let slot = &mut root_table[la64_l3_index(virt.0)];
        if la64_pte_is_branch(*slot) && la64_pte_phys(*slot) == l2.phys {
            *slot = 0;
        }
        Platform::free_pt_node(l2);
    }
}

pub(crate) fn la64_l0_table_mut(root: PhysAddr, virt: VirtAddr) -> Option<&'static mut [u64; 512]> {
    let root = la64_page_table_mut_from_phys(root);
    let l2_pte = root[la64_l3_index(virt.0)];
    if !la64_pte_is_branch(l2_pte) {
        return None;
    }
    let l2 = la64_page_table_mut_from_phys(la64_pte_phys(l2_pte));
    let l1_pte = l2[la64_l2_index(virt.0)];
    if !la64_pte_is_branch(l1_pte) {
        return None;
    }
    let l1 = la64_page_table_mut_from_phys(la64_pte_phys(l1_pte));
    let l0_pte = l1[la64_l1_index(virt.0)];
    if !la64_pte_is_branch(l0_pte) {
        return None;
    }
    Some(la64_page_table_mut_from_phys(la64_pte_phys(l0_pte)))
}

pub(crate) fn la64_l2_table_mut_from_root(
    root: PhysAddr,
    virt: VirtAddr,
) -> Option<&'static mut [u64; 512]> {
    let root = la64_page_table_mut_from_phys(root);
    let pte = root[la64_l3_index(virt.0)];
    if !la64_pte_is_branch(pte) {
        return None;
    }
    Some(la64_page_table_mut_from_phys(la64_pte_phys(pte)))
}

pub(crate) fn la64_l1_table_mut_from_root(
    root: PhysAddr,
    virt: VirtAddr,
) -> Option<&'static mut [u64; 512]> {
    let l2 = la64_l2_table_mut_from_root(root, virt)?;
    let pte = l2[la64_l2_index(virt.0)];
    if !la64_pte_is_branch(pte) {
        return None;
    }
    Some(la64_page_table_mut_from_phys(la64_pte_phys(pte)))
}

pub(crate) fn la64_leaf_slot_mut(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Option<&'static mut u64> {
    match kind {
        PmapReserveKind::Superpage1G => {
            let l2 = la64_l2_table_mut_from_root(root, virt)?;
            Some(&mut l2[la64_l2_index(virt.0)])
        }
        PmapReserveKind::Superpage2M => {
            let l1 = la64_l1_table_mut_from_root(root, virt)?;
            Some(&mut l1[la64_l1_index(virt.0)])
        }
        PmapReserveKind::Page4K => {
            let l0 = la64_l0_table_mut(root, virt)?;
            Some(&mut l0[la64_l0_index(virt.0)])
        }
    }
}

pub(crate) fn write_la64_leaf(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
    leaf: u64,
) -> Result<(), PmapError> {
    let Some(slot) = la64_leaf_slot_mut(root, virt, kind) else {
        return Err(PmapError::InvalidRequest);
    };
    *slot = leaf;
    Ok(())
}

pub(crate) fn unmap_la64_mapping_in_root(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    let Some(slot) = la64_leaf_slot_mut(root, virt, kind) else {
        return Ok(None);
    };
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !la64_pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = la64_pte_phys(current);
    *slot = 0;
    Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
}

pub(crate) fn protect_la64_mapping_in_root(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    let Some(slot) = la64_leaf_slot_mut(root, virt, kind) else {
        return Ok(None);
    };
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !la64_pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let updated = encode_la64_leaf_pte(la64_pte_phys(current), permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, kind.size())))
}

pub(crate) fn release_la64_user_page_table_tree(phys: PhysAddr, level: usize) {
    let table = la64_page_table_mut_from_phys(phys);
    if level > 0 {
        for slot in table.iter_mut() {
            if la64_pte_is_branch(*slot) {
                let child = la64_pte_phys(*slot);
                release_la64_user_page_table_tree(child, level - 1);
                *slot = 0;
                release_la64_committed_pt_node(child);
            }
        }
    }
}

pub(crate) fn validate_la64_user_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    validate_la64_user_virt(virt, kind)?;
    validate_aligned_mapping(virt, phys, kind.size())
}

pub(crate) fn validate_la64_user_virt(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    let end = virt
        .0
        .checked_add(kind.size())
        .ok_or(PmapError::InvalidRequest)?;
    if end > LA64_USER_TOP {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_la64_kernel_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    validate_la64_kernel_virt(virt, kind)?;
    validate_aligned_mapping(virt, phys, kind.size())
}

pub(crate) fn validate_la64_kernel_virt(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    virt.0
        .checked_add(kind.size())
        .ok_or(PmapError::InvalidRequest)?;
    if virt.0 >> (usize::BITS - 1) == 0 {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_aligned_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    size: usize,
) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) || !phys.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_aligned_virt(virt: VirtAddr, size: usize) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn validate_la64_leaf_permissions(
    permissions: PmapPermissions,
    allow_user: bool,
) -> Result<(), PmapError> {
    let readable = permissions.contains(PmapPermissions::READ);
    let writable = permissions.contains(PmapPermissions::WRITE);
    let executable = permissions.contains(PmapPermissions::EXECUTE);
    let user = permissions.contains(PmapPermissions::USER);
    if (user && !allow_user) || (!readable && !executable) || (writable && !readable) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

pub(crate) fn encode_la64_branch_pte(phys: PhysAddr) -> u64 {
    (phys.0 as u64 & LA64_PTE_PFN_MASK) | LA64_PTE_V
}

pub(crate) fn encode_la64_leaf_pte(phys: PhysAddr, permissions: PmapPermissions) -> u64 {
    let mat = if permissions.contains(PmapPermissions::DEVICE) {
        LA64_PTE_MAT_SUC
    } else {
        LA64_PTE_MAT_CC
    };
    let mut flags = LA64_PTE_V | LA64_PTE_A | LA64_PTE_PRESENT | mat;
    if !permissions.contains(PmapPermissions::READ) {
        flags |= LA64_PTE_NR;
    }
    if permissions.contains(PmapPermissions::WRITE) {
        flags |= LA64_PTE_W | LA64_PTE_D | LA64_PTE_M;
    }
    if !permissions.contains(PmapPermissions::EXECUTE) {
        flags |= LA64_PTE_NX;
    }
    if permissions.contains(PmapPermissions::USER) {
        // Keep user leaves at PLV3 without RPLV restriction. Matching the
        // common Linux/LoongArch setup here avoids over-constraining user
        // accesses on pages that must participate in musl's ll/sc atomics.
        flags |= LA64_PTE_PLV_USER;
    }
    if permissions.contains(PmapPermissions::GLOBAL) {
        flags |= LA64_PTE_G;
    }
    (phys.0 as u64 & LA64_PTE_PFN_MASK) | flags
}

pub(crate) fn la64_pte_is_branch(pte: u64) -> bool {
    pte & LA64_PTE_V != 0 && pte & LA64_PTE_PRESENT == 0
}

pub(crate) fn la64_pte_is_leaf(pte: u64) -> bool {
    pte & LA64_PTE_V != 0 && pte & LA64_PTE_PRESENT != 0
}

pub(crate) fn la64_pte_phys(pte: u64) -> PhysAddr {
    PhysAddr((pte & LA64_PTE_PFN_MASK) as usize)
}

pub(crate) fn la64_l3_index(virt: usize) -> usize {
    (virt >> 39) & 0x1ff
}

pub(crate) fn la64_l2_index(virt: usize) -> usize {
    (virt >> 30) & 0x1ff
}

pub(crate) fn la64_l1_index(virt: usize) -> usize {
    (virt >> 21) & 0x1ff
}

pub(crate) fn la64_l0_index(virt: usize) -> usize {
    (virt >> 12) & 0x1ff
}

pub(crate) fn lock_la64_committed_pt_node_registry() -> La64CommittedPtNodeRegistryGuard {
    let mut wait = TlbProgressSpinWait::new();
    while LA64_COMMITTED_PT_NODE_REGISTRY_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spin_with_la64_tlb_progress(&mut wait);
    }
    La64CommittedPtNodeRegistryGuard
}

pub(crate) fn register_la64_committed_pt_node(node: PtNode) {
    let _guard = lock_la64_committed_pt_node_registry();
    let nodes = unsafe { &mut *LA64_COMMITTED_PT_NODES.0.get() };
    let start = la64_committed_pt_node_slot_index(node.phys);
    let mut first_tombstone = None;
    for offset in 0..LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS {
        let index = (start + offset) & (LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS - 1);
        match nodes[index] {
            La64CommittedPtNodeEntry::Empty => {
                let index = first_tombstone.unwrap_or(index);
                nodes[index] = La64CommittedPtNodeEntry::Occupied(node);
                return;
            }
            La64CommittedPtNodeEntry::Tombstone => {
                first_tombstone.get_or_insert(index);
            }
            La64CommittedPtNodeEntry::Occupied(registered) if registered.phys == node.phys => {
                return;
            }
            La64CommittedPtNodeEntry::Occupied(_) => {}
        }
    }
    if let Some(index) = first_tombstone {
        nodes[index] = La64CommittedPtNodeEntry::Occupied(node);
        return;
    }
    panic!(
        "LA64 committed PT-node registry exhausted: entries={}",
        LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS
    );
}

pub(crate) fn register_la64_committed_intermediates(intermediates: PmapReservationIntermediates) {
    if let Some(l2) = intermediates.l2 {
        register_la64_committed_pt_node(l2);
    }
    if let Some(l1) = intermediates.l1 {
        register_la64_committed_pt_node(l1);
    }
    if let Some(l0) = intermediates.l0 {
        register_la64_committed_pt_node(l0);
    }
}

pub(crate) fn take_la64_committed_pt_node(phys: PhysAddr) -> Option<PtNode> {
    let _guard = lock_la64_committed_pt_node_registry();
    let nodes = unsafe { &mut *LA64_COMMITTED_PT_NODES.0.get() };
    let start = la64_committed_pt_node_slot_index(phys);
    for offset in 0..LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS {
        let index = (start + offset) & (LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS - 1);
        match nodes[index] {
            La64CommittedPtNodeEntry::Empty => return None,
            La64CommittedPtNodeEntry::Tombstone => {}
            La64CommittedPtNodeEntry::Occupied(registered) if registered.phys == phys => {
                nodes[index] = La64CommittedPtNodeEntry::Tombstone;
                return Some(registered);
            }
            La64CommittedPtNodeEntry::Occupied(_) => {}
        }
    }
    None
}

#[inline]
pub(crate) fn la64_committed_pt_node_slot_index(phys: PhysAddr) -> usize {
    debug_assert!(LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS.is_power_of_two());
    let page = phys.0 >> 12;
    page.wrapping_mul(0x9e37_79b9_7f4a_7c15usize) & (LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS - 1)
}

pub(crate) fn release_la64_committed_pt_node(phys: PhysAddr) {
    if let Some(node) = take_la64_committed_pt_node(phys) {
        Platform::free_pt_node(node);
    }
}

pub(crate) fn dmw_covers_phys_range(start: PhysAddr, len: usize) -> bool {
    start
        .0
        .checked_add(len)
        .is_some_and(|end| end <= (LA64_PHYS_ADDR_MASK + 1))
}

pub(crate) fn la64_invtlb_global(virt: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "invtlb 0x6, $zero, {virt}",
            virt = in(reg) virt.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = virt;
}

pub(crate) fn la64_invtlb_asid(asid: Asid, virt: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "invtlb 0x5, {asid}, {virt}",
            asid = in(reg) asid.0 as usize,
            virt = in(reg) virt.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = (asid, virt);
}

pub(crate) fn la64_invtlb_all() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("invtlb 0x0, $zero, $zero", options(nostack));
    }

    #[cfg(all(test, not(target_arch = "loongarch64")))]
    run_la64_test_invtlb_all_hook();
}
