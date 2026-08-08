use super::PhysAddr;
#[cfg(target_arch = "loongarch64")]
use super::{la64_cached_virt, la64_irq_trap};

const CPUCFG_CACHE_TOPOLOGY: usize = 16;
const CPUCFG_CACHE_PROPERTIES_BASE: usize = 17;
const MAX_CACHE_OBJECTS: usize = 6;
const HIT_WRITEBACK_INVALIDATE: u8 = 0x10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DataCacheLeaf {
    index: u8,
    line_size: usize,
}

const EMPTY_CACHE_LEAF: DataCacheLeaf = DataCacheLeaf {
    index: 0,
    line_size: 0,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DataCacheTopology {
    leaves: [DataCacheLeaf; MAX_CACHE_OBJECTS],
    len: usize,
}

impl DataCacheTopology {
    fn push(&mut self, index: usize, properties: u32) {
        let line_shift = ((properties >> 24) & 0x7f) as u32;
        let Some(line_size) = 1usize.checked_shl(line_shift) else {
            return;
        };
        if !(16..=4096).contains(&line_size)
            || !line_size.is_power_of_two()
            || index >= MAX_CACHE_OBJECTS
            || self.len == self.leaves.len()
        {
            return;
        }
        self.leaves[self.len] = DataCacheLeaf {
            index: index as u8,
            line_size,
        };
        self.len += 1;
    }
}

fn discover_data_caches(mut read_cpucfg: impl FnMut(usize) -> u32) -> DataCacheTopology {
    let config = read_cpucfg(CPUCFG_CACHE_TOPOLOGY);
    let mut topology = DataCacheTopology {
        leaves: [EMPTY_CACHE_LEAF; MAX_CACHE_OBJECTS],
        len: 0,
    };
    let mut object_index = 0usize;

    let l1_instruction_present = config & (1 << 0) != 0;
    let l1_unified = config & (1 << 1) != 0;
    let l1_data_present = config & (1 << 2) != 0;
    if l1_instruction_present {
        if l1_unified {
            topology.push(
                object_index,
                read_cpucfg(CPUCFG_CACHE_PROPERTIES_BASE + object_index),
            );
        }
        object_index += 1;
    }
    if l1_data_present {
        topology.push(
            object_index,
            read_cpucfg(CPUCFG_CACHE_PROPERTIES_BASE + object_index),
        );
        object_index += 1;
    }

    for level in 0..2 {
        let shift = 3 + level * 7;
        let level_config = config >> shift;
        let instruction_present = level_config & (1 << 0) != 0;
        let unified = level_config & (1 << 1) != 0;
        let data_present = level_config & (1 << 4) != 0;
        if instruction_present {
            if unified {
                topology.push(
                    object_index,
                    read_cpucfg(CPUCFG_CACHE_PROPERTIES_BASE + object_index),
                );
            }
            object_index += 1;
        }
        if data_present {
            topology.push(
                object_index,
                read_cpucfg(CPUCFG_CACHE_PROPERTIES_BASE + object_index),
            );
            object_index += 1;
        }
    }

    topology
}

#[cfg(target_arch = "loongarch64")]
unsafe fn hit_writeback_invalidate(index: u8, address: usize) {
    macro_rules! cache_op {
        ($op:literal) => {
            unsafe {
                core::arch::asm!(
                    concat!("cacop ", stringify!($op), ", {address}, 0"),
                    address = in(reg) address,
                    options(nostack)
                )
            }
        };
    }

    match HIT_WRITEBACK_INVALIDATE | index {
        0x10 => cache_op!(0x10),
        0x11 => cache_op!(0x11),
        0x12 => cache_op!(0x12),
        0x13 => cache_op!(0x13),
        0x14 => cache_op!(0x14),
        0x15 => cache_op!(0x15),
        _ => {}
    }
}

fn aligned_line_span(start: usize, len: usize, line_size: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let last = start.checked_add(len - 1)?;
    Some((
        start & !(line_size - 1),
        (last & !(line_size - 1)).checked_add(line_size)?,
    ))
}

pub(crate) fn writeback_invalidate_range(paddr: PhysAddr, len: usize) {
    #[cfg(target_arch = "loongarch64")]
    {
        let topology = discover_data_caches(la64_irq_trap::read_la64_cpucfg);
        let cached_start = la64_cached_virt(paddr.0);
        la64_irq_trap::la64_dbar();
        for leaf in &topology.leaves[..topology.len] {
            let Some((mut line, end)) = aligned_line_span(cached_start, len, leaf.line_size) else {
                continue;
            };
            while line < end {
                unsafe { hit_writeback_invalidate(leaf.index, line) };
                line += leaf.line_size;
            }
        }
        la64_irq_trap::la64_dbar();
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = (paddr, len, HIT_WRITEBACK_INVALIDATE);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_properties(line_shift: u32) -> u32 {
        line_shift << 24
    }

    #[test]
    fn discovers_separate_l1_data_and_unified_l2() {
        let topology = discover_data_caches(|index| match index {
            16 => (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4),
            17..=19 => cache_properties(6),
            _ => 0,
        });
        assert_eq!(
            &topology.leaves[..topology.len],
            &[
                DataCacheLeaf {
                    index: 1,
                    line_size: 64,
                },
                DataCacheLeaf {
                    index: 2,
                    line_size: 64,
                },
            ]
        );
    }

    #[test]
    fn counts_separate_higher_level_instruction_and_data_objects() {
        let topology = discover_data_caches(|index| match index {
            16 => (1 << 0) | (1 << 2) | (1 << 3) | (1 << 7),
            17..=20 => cache_properties(6),
            _ => 0,
        });
        assert_eq!(topology.len, 2);
        assert_eq!(topology.leaves[0].index, 1);
        assert_eq!(topology.leaves[1].index, 3);
    }

    #[test]
    fn cache_maintenance_span_covers_partial_boundary_lines() {
        assert_eq!(aligned_line_span(0x1021, 64, 64), Some((0x1000, 0x1080)));
        assert_eq!(aligned_line_span(0x1000, 64, 64), Some((0x1000, 0x1040)));
        assert_eq!(aligned_line_span(0x1000, 0, 64), None);
    }
}
