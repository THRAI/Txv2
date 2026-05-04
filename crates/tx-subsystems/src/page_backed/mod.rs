use tx_substrate::epoch::Guard;
use tx_substrate::zone::{Zone, ZoneAllocated};

use crate::mount::structure::MountPayloadPin;
use crate::step::Errno;
use crate::step::StepOutcome;
use crate::vfs::structure::FsObjectId;

pub const FRAME_CAPACITY: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    len: u16,
    bytes: [u8; FRAME_CAPACITY],
}

impl Frame {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() > FRAME_CAPACITY {
            return Err(Errno::Busy);
        }

        let mut frame = Self::zeroed();
        frame.bytes[..bytes.len()].copy_from_slice(bytes);
        frame.len = bytes.len() as u16;
        Ok(frame)
    }

    pub const fn zeroed() -> Self {
        Self {
            len: 0,
            bytes: [0; FRAME_CAPACITY],
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn slice(&self, offset: usize, max_len: usize) -> &[u8] {
        if offset >= self.len() {
            return &[];
        }

        let end = core::cmp::min(self.len(), offset.saturating_add(max_len));
        &self.as_bytes()[offset..end]
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::zeroed()
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
    fn frame_copies_returned_bytes() {
        let frame = Frame::from_bytes(b"hello").expect("frame");

        assert_eq!(frame.len(), 5);
        assert_eq!(frame.as_bytes(), b"hello");
    }

    #[test]
    fn frame_slice_clamps_to_available_bytes() {
        let frame = Frame::from_bytes(b"abcdef").expect("frame");

        assert_eq!(frame.slice(2, 10), b"cdef");
        assert_eq!(frame.slice(6, 1), b"");
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
