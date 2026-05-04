use tx_hal::Ppn;
use tx_substrate::epoch::Guard;
use tx_substrate::zone::{Zone, ZoneAllocated};

use crate::mount::structure::MountPayloadPin;
use crate::step::StepOutcome;
use crate::vfs::structure::FsObjectId;

// Page size constant. Distinct from `Frame`'s surface — a Frame is now a
// PPN handle whose underlying storage is a 4 KiB physical page. Callers
// that compute byte offsets within a page use this constant directly.
pub const FRAME_CAPACITY: usize = 4096;

// Frame is a thin substrate-owned page handle per
// `docs/design/03_memory-vm/PAGE_BACKED_v1.md` §53-91. Liveness lives on
// FrameMeta in the page substrate (cache_ref / map_count / pin_count
// disjunction); the Frame value itself is just the address of the live
// physical page. Filesystem backends that need to materialize a frame
// for `fetch_page` allocate via `tx_substrate::page_allocator` and copy
// disk content through the kernel direct map; the returned `Frame`
// references the resulting PPN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    ppn: Ppn,
}

impl Frame {
    pub const fn new(ppn: Ppn) -> Self {
        Self { ppn }
    }

    pub const fn ppn(self) -> Ppn {
        self.ppn
    }
}

pub struct PageContainer {
    file: Option<FilePageBacking>,
}

pub struct FilePageBacking {
    pub fs_object_id: FsObjectId,
    pub mount_payload_pin: MountPayloadPin,
}

impl PageContainer {
    pub fn file_backing(&self) -> Option<&FilePageBacking> {
        self.file.as_ref()
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn create_file_page_container(
    payload: tx_substrate::zone::Cap<crate::mount::structure::MountPayload>,
    fs_object_id: FsObjectId,
) -> Result<tx_substrate::zone::Cap<PageContainer>, tx_substrate::zone::ZoneError> {
    let reservation = tx_substrate::zone::reserve_for::<PageContainer>()?;
    let pin = MountPayloadPin::acquire(&payload);
    Ok(tx_substrate::zone::sign_for(
        reservation,
        PageContainer {
            file: Some(FilePageBacking {
                fs_object_id,
                mount_payload_pin: pin,
            }),
        },
    ))
}

pub trait FsPageBacking: Send + Sync + 'static {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<Frame>;

    fn flush_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn truncate<'g>(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &'g Guard<'g>,
    ) -> StepOutcome<()>;

    fn fsync<'g>(&self, fs_object_id: FsObjectId, guard: &'g Guard<'g>) -> StepOutcome<()>;

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}

static PAGE_CONTAINER_ZONE: Zone<PageContainer> = Zone::const_new();

unsafe impl ZoneAllocated for PageContainer {
    fn zone() -> &'static Zone<Self> {
        &PAGE_CONTAINER_ZONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::structure::{testing::make_payload_for_test, MountPayload};
    use crate::test_support::EpochTestGuard;
    use core::sync::atomic::Ordering;
    use tx_substrate::epoch;
    use tx_substrate::zone;

    fn setup() {
        tx_substrate::testing::init_host_for_test_once();
        let _ = zone::register_zone_for::<PageContainer>();
        let _ = zone::register_zone_for::<MountPayload>();
    }

    #[test]
    fn file_page_container_keeps_mount_payload_pinned() {
        let _serial = EpochTestGuard::acquire();
        setup();
        let payload = make_payload_for_test();

        assert_eq!(payload.payload_pin_count.load(Ordering::Acquire), 0);

        let pc = create_file_page_container(payload.clone(), FsObjectId(77)).expect("pc");
        assert_eq!(payload.payload_pin_count.load(Ordering::Acquire), 1);

        let guard = epoch::guard();
        let pc_ref = pc.ident_ref(&guard);
        let file = pc_ref.file_backing().expect("file backing");
        assert_eq!(file.fs_object_id, FsObjectId(77));
        assert_eq!(file.mount_payload_pin.payload.raw(), payload.raw());
    }
}
