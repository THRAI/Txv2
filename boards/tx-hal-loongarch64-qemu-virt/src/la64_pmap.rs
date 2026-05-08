use super::la64_irq_trap::{read_la64_csr, write_la64_csr};
use super::*;

pub(crate) fn uart_put_byte(byte: u8) {
    let base = QEMU_LA64_UART0_BASE as *mut u8;

    unsafe {
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(base.add(UART_THR), byte);
    }
}

pub(crate) fn uart_try_get_byte() -> Option<u8> {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let base = QEMU_LA64_UART0_BASE as *const u8;
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
    loop {
        let allocated = LA64_ALLOCATED_ASIDS.load(Ordering::Acquire);
        for asid in 1..u64::BITS {
            let bit = 1u64 << asid;
            if allocated & bit != 0 {
                continue;
            }
            if LA64_ALLOCATED_ASIDS
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
        if allocated == u64::MAX {
            return Err(PmapError::Exhausted);
        }
    }
}

pub(crate) fn free_la64_asid(asid: Asid) {
    if asid.0 == 0 || asid.0 as u32 >= u64::BITS {
        return;
    }
    LA64_ALLOCATED_ASIDS.fetch_and(!(1u64 << asid.0), Ordering::AcqRel);
}

pub(crate) fn activate_la64_pmap(root: &PmapRoot) -> Result<(), PmapError> {
    let pgdh = ensure_la64_kernel_pgdh_root()?;
    configure_la64_page_walk_csrs();

    let asid = root.asid().0 as usize & LA64_ASID_MASK;
    write_la64_csr(LA64_CSR_ASID, asid);
    write_la64_csr(LA64_CSR_PGDL, root.phys().0);
    write_la64_csr(LA64_CSR_PGDH, pgdh.0);

    let crmd = (read_la64_csr(LA64_CSR_CRMD) | LA64_CRMD_PG) & !LA64_CRMD_DA;
    write_la64_csr(LA64_CSR_CRMD, crmd);
    la64_invtlb_all();

    LA64_ACTIVE_ASID.store(asid, Ordering::Release);
    LA64_ACTIVE_PGDL.store(root.phys().0, Ordering::Release);
    LA64_ACTIVE_PGDH.store(pgdh.0, Ordering::Release);
    Ok(())
}

pub(crate) fn ensure_la64_kernel_pgdh_root() -> Result<PhysAddr, PmapError> {
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

pub(crate) fn commit_la64_kernel_mapping(reservation: PmapReservation) {
    let root = PhysAddr(LA64_KERNEL_PGDH_PHYS.load(Ordering::Acquire));
    assert_ne!(root.0, 0, "LA64 kernel PGDH root must exist");
    register_la64_committed_intermediates(reservation.intermediates());
    let leaf = encode_la64_leaf_pte(
        reservation.phys(),
        default_la64_kernel_mapping_permissions(reservation.phys()),
    );
    write_la64_leaf(root, reservation.virt(), reservation.kind(), leaf)
        .expect("reserved LA64 kernel leaf slot");
    la64_invtlb_global(reservation.virt());
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
    let result = unmap_la64_mapping_in_root(PhysAddr(root), virt, kind)?;
    if result.is_some() {
        prune_la64_empty_tables(PhysAddr(root), virt, kind);
    }
    Ok(result)
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

pub(crate) fn default_la64_kernel_mapping_permissions(phys: PhysAddr) -> PmapPermissions {
    let permissions = PmapPermissions::KERNEL_RW;
    if phys.0 >= QEMU_LA64_RAM_END {
        permissions.union(PmapPermissions::DEVICE)
    } else {
        permissions
    }
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
    prune_la64_empty_user_tables(root.phys(), virt);
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

pub(crate) fn prune_la64_empty_user_tables(root: PhysAddr, virt: VirtAddr) {
    prune_la64_empty_tables(root, virt, PmapReserveKind::Page4K);
}

pub(crate) fn prune_la64_empty_tables(root: PhysAddr, virt: VirtAddr, kind: PmapReserveKind) {
    let root_table = la64_page_table_mut_from_phys(root);
    let l2_slot = &mut root_table[la64_l3_index(virt.0)];
    if !la64_pte_is_branch(*l2_slot) {
        return;
    }
    let l2_phys = la64_pte_phys(*l2_slot);
    let l2 = la64_page_table_mut_from_phys(l2_phys);
    if kind == PmapReserveKind::Superpage1G {
        if la64_page_table_is_empty(l2) {
            *l2_slot = 0;
            release_la64_committed_pt_node(l2_phys);
        }
        return;
    }

    let l1_slot = &mut l2[la64_l2_index(virt.0)];
    if !la64_pte_is_branch(*l1_slot) {
        if la64_page_table_is_empty(l2) {
            *l2_slot = 0;
            release_la64_committed_pt_node(l2_phys);
        }
        return;
    }
    let l1_phys = la64_pte_phys(*l1_slot);
    let l1 = la64_page_table_mut_from_phys(l1_phys);
    if kind == PmapReserveKind::Superpage2M {
        if la64_page_table_is_empty(l1) {
            *l1_slot = 0;
            release_la64_committed_pt_node(l1_phys);
        }
        if la64_page_table_is_empty(l2) {
            *l2_slot = 0;
            release_la64_committed_pt_node(l2_phys);
        }
        return;
    }

    let l0_slot = &mut l1[la64_l1_index(virt.0)];
    if !la64_pte_is_branch(*l0_slot) {
        if la64_page_table_is_empty(l1) {
            *l1_slot = 0;
            release_la64_committed_pt_node(l1_phys);
        }
        if la64_page_table_is_empty(l2) {
            *l2_slot = 0;
            release_la64_committed_pt_node(l2_phys);
        }
        return;
    }
    let l0_phys = la64_pte_phys(*l0_slot);
    let l0 = la64_page_table_mut_from_phys(l0_phys);
    if la64_page_table_is_empty(l0) {
        *l0_slot = 0;
        release_la64_committed_pt_node(l0_phys);
    }
    if la64_page_table_is_empty(l1) {
        *l1_slot = 0;
        release_la64_committed_pt_node(l1_phys);
    }
    if la64_page_table_is_empty(l2) {
        *l2_slot = 0;
        release_la64_committed_pt_node(l2_phys);
    }
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
        flags |= LA64_PTE_PLV_USER | LA64_PTE_RPLV;
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

pub(crate) fn la64_page_table_is_empty(table: &[u64; 512]) -> bool {
    table.iter().all(|entry| *entry == 0)
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
    while LA64_COMMITTED_PT_NODE_REGISTRY_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    La64CommittedPtNodeRegistryGuard
}

pub(crate) fn register_la64_committed_pt_node(node: PtNode) {
    let _guard = lock_la64_committed_pt_node_registry();
    let nodes = unsafe { &mut *LA64_COMMITTED_PT_NODES.0.get() };
    for slot in nodes.iter_mut() {
        if slot.is_some_and(|registered| registered.phys == node.phys) {
            return;
        }
    }
    for slot in nodes.iter_mut() {
        if slot.is_none() {
            *slot = Some(node);
            return;
        }
    }
    panic!("LA64 committed PT-node registry exhausted");
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
    for slot in nodes.iter_mut() {
        if slot.is_some_and(|registered| registered.phys == phys) {
            return slot.take();
        }
    }
    None
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
}

pub(crate) fn la64_read_stable_counter() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let ticks: u64;
        unsafe {
            core::arch::asm!(
                "rdtime.d {ticks}, $zero",
                ticks = out(reg) ticks,
                options(nomem, nostack)
            );
        }
        ticks
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        0
    }
}

pub(crate) fn la64_current_cpu_id() -> CpuId {
    let kernel_tls = la64_read_kernel_tls();
    if kernel_tls < LA64_MAX_BOOT_CPUS {
        CpuId(kernel_tls)
    } else {
        CpuId(0)
    }
}

pub(crate) fn la64_read_kernel_tls() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: usize;
        unsafe {
            core::arch::asm!(
                "move {value}, $r21",
                value = out(reg) value,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.load(Ordering::Acquire)
    }
}

pub(crate) fn la64_write_kernel_tls(value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $r21, {value}", value = in(reg) value, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.store(value, Ordering::Release);
    }
}

pub(crate) unsafe fn la64_install_kernel_stack(top: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $sp, {top}", top = in(reg) top.0, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = top;
}

pub(crate) fn la64_wait_for_interrupt_once() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("idle 0", options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    core::hint::spin_loop();
}
