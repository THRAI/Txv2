/// The timekeeper-to-VVAR publication payload.
///
/// The payload keeps its existing field shape while Phase 3 moves the mapped
/// page writer to [`super::VvarData`]. This lets current publishers migrate
/// without creating a second ABI layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VvarSnapshot {
    pub realtime_generation: u64,
    pub cycle_last: u64,
    pub mask: u64,
    pub mult: u64,
    pub shift: u64,
    pub realtime_sec: u64,
    pub realtime_nsec_shifted: u64,
    pub monotonic_sec: u64,
    pub monotonic_nsec_shifted: u64,
}
