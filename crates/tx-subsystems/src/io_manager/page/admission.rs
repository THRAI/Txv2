//! Page-request admission helpers for the staged L4 service path.

use crate::io_manager::page::{
    PageContainerKey, PageGeneration, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
    PageIoRequestId, PageRequestQueue,
};
use crate::page_backed::{PageSlot, PageSlotFetch, PageSlotState};
use tx_hal::Ppn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageAdmission {
    Queued {
        id: PageIoRequestId,
        generation: PageGeneration,
    },
    Joined {
        generation: PageGeneration,
    },
    Resident {
        ppn: Ppn,
        generation: PageGeneration,
    },
    Blocked {
        state: PageSlotState,
        generation: PageGeneration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageAdmissionError {
    QueueFull,
}

pub fn admit_demand_read(
    slot: &PageSlot,
    queue: &mut PageRequestQueue,
    pc: PageContainerKey,
    page: u64,
) -> Result<PageAdmission, PageAdmissionError> {
    match slot.snapshot().state {
        PageSlotState::Resident { ppn } | PageSlotState::Dirty { ppn } => {
            return Ok(PageAdmission::Resident {
                ppn,
                generation: slot.generation(),
            });
        }
        PageSlotState::Fetching => {
            return Ok(PageAdmission::Joined {
                generation: slot.generation(),
            });
        }
        PageSlotState::Writeback { .. } => {
            let snapshot = slot.snapshot();
            return Ok(PageAdmission::Blocked {
                state: snapshot.state,
                generation: snapshot.generation,
            });
        }
        PageSlotState::Empty | PageSlotState::Error { .. } => {}
    }

    if !queue.has_capacity() {
        return Err(PageAdmissionError::QueueFull);
    }

    match slot.begin_fetch() {
        PageSlotFetch::Owner { generation } => {
            let id = queue
                .submit(
                    pc,
                    PageIoRange::new(page, 1),
                    PageIoOp::Read,
                    PageIoPriority::Demand,
                    PageIoFlags::DEMAND,
                    Some(generation),
                )
                .map_err(|_| PageAdmissionError::QueueFull)?;
            Ok(PageAdmission::Queued { id, generation })
        }
        PageSlotFetch::Joined { generation } => Ok(PageAdmission::Joined { generation }),
        PageSlotFetch::Resident { ppn, generation } => {
            Ok(PageAdmission::Resident { ppn, generation })
        }
        PageSlotFetch::Blocked { state, generation } => {
            Ok(PageAdmission::Blocked { state, generation })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_manager::page::{
        PageContainerKey, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange, PageRequestQueue,
    };
    use crate::page_backed::{PageSlot, PageSlotFetch, PageSlotState};
    use tx_hal::Ppn;

    #[test]
    fn page_admission_queues_owner_fetch_with_generation_hint() {
        let pc = PageContainerKey::new(7);
        let slot = PageSlot::new();
        let mut queue = PageRequestQueue::new(2);

        let outcome = admit_demand_read(&slot, &mut queue, pc, 4).expect("admit miss");
        let PageAdmission::Queued { id, generation } = outcome else {
            panic!("empty slot should enqueue an owned fetch");
        };

        assert_eq!(slot.begin_fetch(), PageSlotFetch::Joined { generation });
        let request = queue.pop_next().expect("queued request");
        assert_eq!(request.id, id);
        assert_eq!(request.pc, pc);
        assert_eq!(request.range, PageIoRange::new(4, 1));
        assert_eq!(request.op, PageIoOp::Read);
        assert_eq!(request.priority, PageIoPriority::Demand);
        assert!(request.flags.contains(PageIoFlags::DEMAND));
        assert_eq!(request.generation_hint, Some(generation));
    }

    #[test]
    fn page_admission_joins_existing_fetch_without_duplicate_request() {
        let pc = PageContainerKey::new(7);
        let slot = PageSlot::new();
        let mut queue = PageRequestQueue::new(2);

        let PageAdmission::Queued { generation, .. } =
            admit_demand_read(&slot, &mut queue, pc, 4).expect("first miss")
        else {
            panic!("first miss should own fetch");
        };
        assert_eq!(
            admit_demand_read(&slot, &mut queue, pc, 4).expect("second miss"),
            PageAdmission::Joined { generation }
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn page_admission_reports_resident_without_queueing() {
        let pc = PageContainerKey::new(7);
        let slot = PageSlot::new();
        let mut queue = PageRequestQueue::new(2);
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("setup owner");
        };
        slot.complete_fetch(generation, Ok(Ppn(55)))
            .expect("install resident page");

        assert_eq!(
            admit_demand_read(&slot, &mut queue, pc, 4).expect("resident"),
            PageAdmission::Resident {
                ppn: Ppn(55),
                generation,
            }
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn page_admission_rejects_queue_full_without_claiming_slot() {
        let pc = PageContainerKey::new(7);
        let slot = PageSlot::new();
        let mut queue = PageRequestQueue::new(1);
        queue
            .submit(
                pc,
                PageIoRange::new(99, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                None,
            )
            .expect("fill queue");

        assert_eq!(
            admit_demand_read(&slot, &mut queue, pc, 4),
            Err(PageAdmissionError::QueueFull)
        );
        assert_eq!(slot.snapshot().state, PageSlotState::Empty);
    }
}
