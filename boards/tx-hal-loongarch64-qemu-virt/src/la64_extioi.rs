use super::la64_irq_trap::la64_dbar;
use super::*;

const LA64_EIOINTC_BITMAP_WORDS: usize = LA64_EIOINTC_IRQS as usize / u64::BITS as usize;
static LA64_EXTIOI_CLAIMED: [AtomicU64; LA64_EIOINTC_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; LA64_EIOINTC_BITMAP_WORDS];
static LA64_EXTIOI_DUPLICATE_SUPPRESSED: [AtomicU64; LA64_EIOINTC_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; LA64_EIOINTC_BITMAP_WORDS];

#[cfg(test)]
pub(crate) fn reset_la64_extioi_claim_state_for_test() {
    for claimed in &LA64_EXTIOI_CLAIMED {
        claimed.store(0, Ordering::Release);
    }
    for suppressed in &LA64_EXTIOI_DUPLICATE_SUPPRESSED {
        suppressed.store(0, Ordering::Release);
    }
}

pub(crate) fn la64_extioi_claim() -> u32 {
    for word in 0..(LA64_EIOINTC_IRQS / u64::BITS) {
        let offset = word as usize * core::mem::size_of::<u64>();
        let pending = la64_eiointc_read_u64(LA64_EIOINTC_COREISR_START + offset)
            & la64_eiointc_read_enable_u64(word);
        if pending == 0 {
            continue;
        }

        let ext_irq = word * u64::BITS + pending.trailing_zeros();
        let bit = 1u64 << (ext_irq % u64::BITS);
        let claimed = &LA64_EXTIOI_CLAIMED[word as usize];
        if claimed.fetch_or(bit, Ordering::AcqRel) & bit == 0 {
            return la64_public_irq_from_extioi(ext_irq);
        }

        // ExtIOI exposes a non-destructive pending bitmap rather than the
        // claim/complete gateway provided by a PLIC.  A level-triggered source
        // can therefore be presented again while its deferred bottom half
        // still owns the first logical claim.  Acknowledge that duplicate
        // presentation, then suppress only the ExtIOI delivery line until the
        // original owner completes.  The PCH-PIC mask remains untouched so an
        // explicit `IrqIf::mask` stays distinguishable.
        let public_irq = la64_public_irq_from_extioi(ext_irq);
        la64_complete_external_irq_raw(public_irq);
        la64_eiointc_clear_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
        LA64_EXTIOI_DUPLICATE_SUPPRESSED[word as usize].fetch_or(bit, Ordering::Release);
        return 0;
    }

    0
}

pub(crate) fn la64_complete_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_complete_external_irq_raw(irq);

    let word = ext_irq as usize / u64::BITS as usize;
    let bit = 1u64 << (ext_irq % u64::BITS);
    LA64_EXTIOI_CLAIMED[word].fetch_and(!bit, Ordering::AcqRel);
    if LA64_EXTIOI_DUPLICATE_SUPPRESSED[word].fetch_and(!bit, Ordering::AcqRel) & bit != 0 {
        // Balance only the temporary ExtIOI suppression used for a duplicate
        // presentation.  Do not unmask the PCH-PIC input here: dispatch may
        // have explicitly masked an unhandled source, and that policy must
        // outlive controller completion.
        la64_dbar();
        la64_eiointc_set_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
    }
}

fn la64_complete_external_irq_raw(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_eiointc_write_bit(LA64_EIOINTC_COREISR_START, ext_irq);
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        la64_pch_pic_write_bit(LA64_PCH_PIC_CLEAR_START, pin);
    }
}

pub(crate) fn la64_mask_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_eiointc_clear_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        la64_pch_pic_set_bit(LA64_PCH_PIC_MASK_START, pin);
    }
}

pub(crate) fn la64_unmask_external_irq(irq: u32) {
    let Some(ext_irq) = la64_extioi_irq_from_public_irq(irq) else {
        return;
    };
    la64_eiointc_set_bit(LA64_EIOINTC_ENABLE_START, ext_irq);
    if let Some(pin) = la64_pch_pic_pin_from_public_irq(irq) {
        // QEMU resets every PCH-PIC HTMSI vector entry to zero. Program the
        // pin-to-ExtIOI route before exposing a pending PCH source; otherwise
        // all uninitialised pins are delivered as ExtIOI 0 even though the HAL
        // enables and claims the same-numbered ExtIOI line.
        la64_pch_pic_write_u8(
            LA64_PCH_PIC_HTMSI_VECTOR_START + pin as usize,
            ext_irq as u8,
        );
        la64_dbar();
        la64_pch_pic_clear_bit(LA64_PCH_PIC_MASK_START, pin);
    }
}

pub(crate) const fn la64_public_irq_from_extioi(ext_irq: u32) -> u32 {
    if ext_irq < QEMU_LA64_PCH_PIC_IRQS {
        QEMU_LA64_GSI_BASE + ext_irq
    } else {
        ext_irq
    }
}

pub(crate) const fn la64_extioi_irq_from_public_irq(irq: u32) -> Option<u32> {
    if irq >= QEMU_LA64_GSI_BASE && irq < QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS {
        Some(irq - QEMU_LA64_GSI_BASE)
    } else if irq < LA64_EIOINTC_IRQS {
        Some(irq)
    } else {
        None
    }
}

pub(crate) const fn la64_pch_pic_pin_from_public_irq(irq: u32) -> Option<u32> {
    if irq >= QEMU_LA64_GSI_BASE && irq < QEMU_LA64_GSI_BASE + QEMU_LA64_PCH_PIC_IRQS {
        Some(irq - QEMU_LA64_GSI_BASE)
    } else {
        None
    }
}

pub(crate) fn la64_eiointc_set_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (irq % u32::BITS);
    let value = la64_eiointc_read_u32(offset) | bit;
    la64_eiointc_write_u32(offset, value);
}

pub(crate) fn la64_eiointc_clear_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (irq % u32::BITS);
    let value = la64_eiointc_read_u32(offset) & !bit;
    la64_eiointc_write_u32(offset, value);
}

pub(crate) fn la64_eiointc_write_bit(start: usize, irq: u32) {
    let offset = start + (irq as usize / u64::BITS as usize) * core::mem::size_of::<u64>();
    let bit = 1u64 << (irq % u64::BITS);
    la64_eiointc_write_u64(offset, bit);
}

pub(crate) fn la64_eiointc_read_enable_u64(word: u32) -> u64 {
    let offset = LA64_EIOINTC_ENABLE_START + word as usize * core::mem::size_of::<u64>();
    let lo = la64_eiointc_read_u32(offset) as u64;
    let hi = la64_eiointc_read_u32(offset + core::mem::size_of::<u32>()) as u64;
    lo | (hi << u32::BITS)
}

pub(crate) fn la64_eiointc_read_u32(offset: usize) -> u32 {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u32;
        unsafe {
            core::arch::asm!(
                "iocsrrd.w {value}, {address}",
                value = out(reg) value,
                address = in(reg) address,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire) as u32,
            offset if offset == LA64_EIOINTC_ENABLE_START + core::mem::size_of::<u32>() => {
                (LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire) >> u32::BITS) as u32
            }
            _ => {
                let _ = address;
                0
            }
        }
    }
}

pub(crate) fn la64_eiointc_write_u32(offset: usize, value: u32) {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "iocsrwr.w {value}, {address}",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => {
                LA64_HOST_EIOINTC_ENABLE0.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & !(u32::MAX as u64)) | value as u64)
                })
            }
            offset if offset == LA64_EIOINTC_ENABLE_START + core::mem::size_of::<u32>() => {
                LA64_HOST_EIOINTC_ENABLE0.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & (u32::MAX as u64)) | ((value as u64) << u32::BITS))
                })
            }
            _ => {
                let _ = (address, value);
                Ok(0)
            }
        }
        .ok();
    }
}

pub(crate) fn la64_eiointc_read_u64(offset: usize) -> u64 {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u64;
        unsafe {
            core::arch::asm!(
                "iocsrrd.d {value}, {address}",
                value = out(reg) value,
                address = in(reg) address,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.load(Ordering::Acquire),
            LA64_EIOINTC_COREISR_START => LA64_HOST_EIOINTC_COREISR0.load(Ordering::Acquire),
            _ => {
                let _ = address;
                0
            }
        }
    }
}

pub(crate) fn la64_eiointc_write_u64(offset: usize, value: u64) {
    let address = LA64_EIOINTC_BASE + offset;
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "iocsrwr.d {value}, {address}",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_EIOINTC_ENABLE_START => LA64_HOST_EIOINTC_ENABLE0.store(value, Ordering::Release),
            LA64_EIOINTC_COREISR_START => {
                LA64_HOST_EIOINTC_COREISR0.fetch_and(!value, Ordering::AcqRel);
            }
            _ => {
                let _ = (address, value);
            }
        }
    }
}

pub(crate) fn la64_pch_pic_set_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    let value = la64_pch_pic_read_u32(offset) | bit;
    la64_pch_pic_write_u32(offset, value);
}

pub(crate) fn la64_pch_pic_clear_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    let value = la64_pch_pic_read_u32(offset) & !bit;
    la64_pch_pic_write_u32(offset, value);
}

pub(crate) fn la64_pch_pic_write_bit(start: usize, pin: u32) {
    let offset = start + (pin as usize / u32::BITS as usize) * core::mem::size_of::<u32>();
    let bit = 1u32 << (pin % u32::BITS);
    la64_pch_pic_write_u32(offset, bit);
}

pub(crate) fn la64_pch_pic_read_u32(offset: usize) -> u32 {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::ptr::read_volatile(la64_uncached_virt(QEMU_LA64_PCH_PIC_BASE + offset) as *const u32)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        match offset {
            LA64_PCH_PIC_MASK_START => LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) as u32,
            offset if offset == LA64_PCH_PIC_MASK_START + core::mem::size_of::<u32>() => {
                (LA64_HOST_PCH_PIC_MASK.load(Ordering::Acquire) >> u32::BITS) as u32
            }
            _ => 0,
        }
    }
}

pub(crate) fn la64_pch_pic_write_u32(offset: usize, value: u32) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::ptr::write_volatile(
            la64_uncached_virt(QEMU_LA64_PCH_PIC_BASE + offset) as *mut u32,
            value,
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    match offset {
        LA64_PCH_PIC_MASK_START => {
            LA64_HOST_PCH_PIC_MASK
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & !(u32::MAX as u64)) | value as u64)
                })
                .ok();
        }
        offset if offset == LA64_PCH_PIC_MASK_START + core::mem::size_of::<u32>() => {
            LA64_HOST_PCH_PIC_MASK
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                    Some((old & (u32::MAX as u64)) | ((value as u64) << u32::BITS))
                })
                .ok();
        }
        LA64_PCH_PIC_CLEAR_START => {
            LA64_HOST_EIOINTC_COREISR0.fetch_and(!(value as u64), Ordering::AcqRel);
        }
        offset if offset == LA64_PCH_PIC_CLEAR_START + core::mem::size_of::<u32>() => {
            LA64_HOST_EIOINTC_COREISR0.fetch_and(!((value as u64) << u32::BITS), Ordering::AcqRel);
        }
        _ => {
            let _ = value;
        }
    }
}

pub(crate) fn la64_pch_pic_write_u8(offset: usize, value: u8) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::ptr::write_volatile(
            la64_uncached_virt(QEMU_LA64_PCH_PIC_BASE + offset) as *mut u8,
            value,
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    if let Some(pin) = offset.checked_sub(LA64_PCH_PIC_HTMSI_VECTOR_START) {
        if let Some(vector) = LA64_HOST_PCH_PIC_HTMSI_VECTOR.get(pin) {
            vector.store(value, Ordering::Release);
        }
    }
}
