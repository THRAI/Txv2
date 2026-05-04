use crate::step::Errno;
use crate::vfs::checks::resolution::state::WalkMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkCause {
    MissingComponent,
    NonDirectoryIntermediate,
    #[cfg(test)]
    SymlinkBudgetExceeded,
    NameTooLong,
    DetachedNamespace,
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    StaleObservation,
}

pub(crate) fn classify(_mode: WalkMode, cause: WalkCause) -> Errno {
    match cause {
        WalkCause::MissingComponent => Errno::NoEntry,
        WalkCause::NonDirectoryIntermediate => Errno::NotDirectory,
        #[cfg(test)]
        WalkCause::SymlinkBudgetExceeded => Errno::TooManySymlinks,
        WalkCause::NameTooLong => Errno::NameTooLong,
        WalkCause::DetachedNamespace => Errno::NoEntry,
        #[cfg(any(test, feature = "vfs-read-test-support"))]
        WalkCause::StaleObservation => Errno::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_walk_causes_to_errno() {
        assert_eq!(
            classify(WalkMode::Entity, WalkCause::MissingComponent),
            Errno::NoEntry
        );
        assert_eq!(
            classify(WalkMode::Entity, WalkCause::SymlinkBudgetExceeded),
            Errno::TooManySymlinks
        );
        assert_eq!(
            classify(WalkMode::Entity, WalkCause::StaleObservation),
            Errno::Stale
        );
    }
}
