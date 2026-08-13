//! Fail-closed parser for the four primary entries in a legacy MBR.

pub const MBR_SECTOR_SIZE: usize = 512;

const PARTITION_TABLE_OFFSET: usize = 446;
const PARTITION_ENTRY_SIZE: usize = 16;
const PRIMARY_PARTITION_COUNT: usize = 4;
const PROTECTIVE_GPT_TYPE: u8 = 0xee;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MbrPartition {
    pub number: u8,
    pub bootable: bool,
    pub partition_type: u8,
    pub start_lba: u64,
    pub len_lba: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MbrPartitionTable {
    entries: [Option<MbrPartition>; PRIMARY_PARTITION_COUNT],
}

impl MbrPartitionTable {
    pub fn get(&self, number: u8) -> Option<MbrPartition> {
        let index = usize::from(number.checked_sub(1)?);
        self.entries.get(index).copied().flatten()
    }

    pub fn iter(&self) -> impl Iterator<Item = MbrPartition> + '_ {
        self.entries.iter().flatten().copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MbrParseError {
    Truncated,
    InvalidSignature,
    InvalidStatus {
        number: u8,
        status: u8,
    },
    InvalidEntry {
        number: u8,
    },
    ProtectiveGpt {
        number: u8,
    },
    ArithmeticOverflow {
        number: u8,
    },
    OutOfBounds {
        number: u8,
        end_lba: u64,
        parent_len_lba: u64,
    },
    OverlappingPartitions {
        first: u8,
        second: u8,
    },
    NoPartitions,
}

/// Parse the four legacy primary-partition entries from the first disk sector.
///
/// Empty rows must be entirely zero. Populated rows require a conventional
/// status flag, a non-zero type/start/length, an addressable inclusive final
/// LBA, and an exclusive end within the parent device. A protective GPT row is
/// rejected so a caller cannot accidentally mount it as an ordinary slice.
pub fn parse_mbr(sector: &[u8], parent_len_lba: u64) -> Result<MbrPartitionTable, MbrParseError> {
    if sector.len() < MBR_SECTOR_SIZE {
        return Err(MbrParseError::Truncated);
    }
    if sector[510..512] != [0x55, 0xaa] {
        return Err(MbrParseError::InvalidSignature);
    }

    let mut entries = [None; PRIMARY_PARTITION_COUNT];
    let mut populated = 0usize;
    for (index, slot) in entries.iter_mut().enumerate() {
        let number = (index + 1) as u8;
        let offset = PARTITION_TABLE_OFFSET + index * PARTITION_ENTRY_SIZE;
        let raw = &sector[offset..offset + PARTITION_ENTRY_SIZE];
        if raw.iter().all(|byte| *byte == 0) {
            continue;
        }

        let status = raw[0];
        if status != 0 && status != 0x80 {
            return Err(MbrParseError::InvalidStatus { number, status });
        }
        let partition_type = raw[4];
        let start_lba = u32::from_le_bytes(raw[8..12].try_into().unwrap());
        let len_lba = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        if partition_type == 0 || start_lba == 0 || len_lba == 0 {
            return Err(MbrParseError::InvalidEntry { number });
        }
        if partition_type == PROTECTIVE_GPT_TYPE {
            return Err(MbrParseError::ProtectiveGpt { number });
        }
        start_lba
            .checked_add(len_lba - 1)
            .ok_or(MbrParseError::ArithmeticOverflow { number })?;
        let start_lba = u64::from(start_lba);
        let len_lba = u64::from(len_lba);
        let end_lba = start_lba
            .checked_add(len_lba)
            .ok_or(MbrParseError::ArithmeticOverflow { number })?;
        if end_lba > parent_len_lba {
            return Err(MbrParseError::OutOfBounds {
                number,
                end_lba,
                parent_len_lba,
            });
        }

        *slot = Some(MbrPartition {
            number,
            bootable: status == 0x80,
            partition_type,
            start_lba,
            len_lba,
        });
        populated += 1;
    }

    if populated == 0 {
        return Err(MbrParseError::NoPartitions);
    }

    for first_index in 0..PRIMARY_PARTITION_COUNT {
        let Some(first) = entries[first_index] else {
            continue;
        };
        let first_end = first.start_lba.checked_add(first.len_lba).ok_or(
            MbrParseError::ArithmeticOverflow {
                number: first.number,
            },
        )?;
        for second in entries.iter().skip(first_index + 1).flatten() {
            let second_end = second.start_lba.checked_add(second.len_lba).ok_or(
                MbrParseError::ArithmeticOverflow {
                    number: second.number,
                },
            )?;
            if first.start_lba < second_end && second.start_lba < first_end {
                return Err(MbrParseError::OverlappingPartitions {
                    first: first.number,
                    second: second.number,
                });
            }
        }
    }

    Ok(MbrPartitionTable { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector_with_partition(
        status: u8,
        partition_type: u8,
        start_lba: u32,
        len_lba: u32,
    ) -> [u8; MBR_SECTOR_SIZE] {
        let mut sector = [0u8; MBR_SECTOR_SIZE];
        sector[510..512].copy_from_slice(&[0x55, 0xaa]);
        write_partition(&mut sector, 1, status, partition_type, start_lba, len_lba);
        sector
    }

    fn write_partition(
        sector: &mut [u8; MBR_SECTOR_SIZE],
        number: u8,
        status: u8,
        partition_type: u8,
        start_lba: u32,
        len_lba: u32,
    ) {
        let offset = 446 + usize::from(number - 1) * 16;
        let entry = &mut sector[offset..offset + 16];
        entry[0] = status;
        entry[4] = partition_type;
        entry[8..12].copy_from_slice(&start_lba.to_le_bytes());
        entry[12..16].copy_from_slice(&len_lba.to_le_bytes());
    }

    #[test]
    fn parses_one_based_primary_partition_geometry() {
        let sector = sector_with_partition(0x80, 0x83, 4_194_367, 58_338_929);

        let table = parse_mbr(&sector, 62_533_296).expect("valid LA disk MBR");
        let root = table.get(1).expect("sda1");

        assert_eq!(root.number, 1);
        assert!(root.bootable);
        assert_eq!(root.partition_type, 0x83);
        assert_eq!(root.start_lba, 4_194_367);
        assert_eq!(root.len_lba, 58_338_929);
    }

    #[test]
    fn parses_all_four_primary_slots() {
        let mut sector = sector_with_partition(0x80, 0x83, 1, 8);
        write_partition(&mut sector, 2, 0, 0x82, 9, 8);
        write_partition(&mut sector, 3, 0, 0x83, 17, 8);
        write_partition(&mut sector, 4, 0, 0x0c, 25, 8);

        let table = parse_mbr(&sector, 33).expect("four valid primary rows");

        assert_eq!(table.iter().count(), 4);
        assert_eq!(table.get(2).unwrap().partition_type, 0x82);
        assert_eq!(table.get(4).unwrap().start_lba, 25);
        assert_eq!(table.get(0), None);
        assert_eq!(table.get(5), None);
    }

    #[test]
    fn rejects_overlapping_nonempty_primary_partitions() {
        let mut sector = sector_with_partition(0x80, 0x83, 10, 10);
        // Slot 2 remains empty; slot 3 overlaps slot 1 at LBA 19.
        write_partition(&mut sector, 3, 0, 0x82, 19, 4);

        assert_eq!(
            parse_mbr(&sector, 32),
            Err(MbrParseError::OverlappingPartitions {
                first: 1,
                second: 3,
            })
        );
    }

    #[test]
    fn rejects_signature_status_partial_empty_and_protective_gpt() {
        assert_eq!(parse_mbr(&[0u8; 511], 8), Err(MbrParseError::Truncated));

        let mut empty = [0u8; MBR_SECTOR_SIZE];
        empty[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(parse_mbr(&empty, 8), Err(MbrParseError::NoPartitions));

        let mut sector = sector_with_partition(0, 0x83, 1, 1);
        sector[510] = 0;
        assert_eq!(parse_mbr(&sector, 8), Err(MbrParseError::InvalidSignature));

        let sector = sector_with_partition(0x7f, 0x83, 1, 1);
        assert_eq!(
            parse_mbr(&sector, 8),
            Err(MbrParseError::InvalidStatus {
                number: 1,
                status: 0x7f,
            })
        );

        let sector = sector_with_partition(0, 0, 1, 1);
        assert_eq!(
            parse_mbr(&sector, 8),
            Err(MbrParseError::InvalidEntry { number: 1 })
        );

        let sector = sector_with_partition(0, 0xee, 1, 7);
        assert_eq!(
            parse_mbr(&sector, 8),
            Err(MbrParseError::ProtectiveGpt { number: 1 })
        );
    }

    #[test]
    fn rejects_partition_end_overflow_and_parent_overrun() {
        let sector = sector_with_partition(0, 0x83, u32::MAX, 2);
        assert_eq!(
            parse_mbr(&sector, u64::MAX),
            Err(MbrParseError::ArithmeticOverflow { number: 1 })
        );

        let sector = sector_with_partition(0, 0x83, 7, 2);
        assert_eq!(
            parse_mbr(&sector, 8),
            Err(MbrParseError::OutOfBounds {
                number: 1,
                end_lba: 9,
                parent_len_lba: 8,
            })
        );
    }
}
