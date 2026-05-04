use alloc::boxed::Box;
use tx_substrate::epoch::Guard;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use tx_substrate::zone::Cap;

use crate::step::Errno;
use crate::vfs::checks::resolution::error;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use crate::vfs::checks::resolution::state::ResumeToken;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use crate::vfs::checks::resolution::state::TrailEntry;
use crate::vfs::checks::resolution::state::{WalkMode, WalkState};
#[cfg(any(test, feature = "vfs-read-test-support"))]
use crate::vfs::checks::resolution::step::IORequest;
use crate::vfs::checks::resolution::step::{self, KernelStep};
use crate::vfs::checks::resolution::terminal;
use crate::vfs::checks::witness::WalkWitness;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use crate::vfs::structure::{DEntry, DEntryChildLookup};

pub(crate) enum DriverStep<'g> {
    Accept(Box<WalkWitness<'g>>),
    Error(Errno),
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    NeedIO(Box<IORequest>, Box<ResumeToken>),
    #[cfg(not(any(test, feature = "vfs-read-test-support")))]
    NeedIO,
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) enum IOResult {
    ChildFound(Cap<DEntry>),
}

pub(crate) fn run_walker<'a, 'g>(
    mode: WalkMode,
    mut state: WalkState<'a, 'g>,
    guard: &'g Guard<'_>,
) -> DriverStep<'g> {
    let policy = mode.final_symlink_policy();
    loop {
        if mode.is_parent_mode() && state.remaining.count() == 1 {
            return match terminal::build_penultimate_witness(mode, state, guard) {
                Ok(witness) => DriverStep::Accept(Box::new(witness)),
                Err(errno) => DriverStep::Error(errno),
            };
        }

        if terminal::accepts(mode, &state, guard) {
            return match terminal::build_witness(mode, state, guard) {
                Ok(witness) => DriverStep::Accept(Box::new(witness)),
                Err(errno) => DriverStep::Error(errno),
            };
        }

        match step::kernel_step(state, policy, guard) {
            KernelStep::Continue(next) => state = *next,
            #[cfg(any(test, feature = "vfs-read-test-support"))]
            KernelStep::NeedIO(request, token) => return DriverStep::NeedIO(request, token),
            #[cfg(not(any(test, feature = "vfs-read-test-support")))]
            KernelStep::NeedIO => return DriverStep::NeedIO,
            KernelStep::Error(cause) => return DriverStep::Error(error::classify(mode, cause)),
        }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn resume_walker<'g>(
    mode: WalkMode,
    token: ResumeToken,
    io_result: IOResult,
    guard: &'g Guard<'_>,
) -> DriverStep<'g> {
    let state = match apply_io_result(&token, io_result, guard) {
        Ok(state) => state,
        Err(errno) => return DriverStep::Error(errno),
    };
    run_walker(mode, state, guard)
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) fn apply_io_result<'t, 'g>(
    token: &'t ResumeToken,
    io_result: IOResult,
    guard: &'g Guard<'_>,
) -> Result<WalkState<'t, 'g>, Errno> {
    match token {
        ResumeToken::LookupChild {
            suspended,
            parent,
            name,
        } => {
            let mut resumed = suspended.resume(guard);
            let IOResult::ChildFound(child) = io_result;

            let parent_ref = parent.ident_ref(guard);
            match parent_ref.children.lookup(name, guard) {
                DEntryChildLookup::Found(existing) => {
                    resumed
                        .trail
                        .push(TrailEntry::DEntry(resumed.cursor))
                        .map_err(|_| Errno::ENAMETOOLONG)?;
                    resumed.cursor = (*existing).into_ident_ref();
                    Ok(resumed)
                }
                DEntryChildLookup::Missing => {
                    let reservation = parent_ref
                        .children
                        .reserve_insert(name.clone())
                        .map_err(map_children_insert_error)?;
                    reservation.commit(child);
                    let installed = match parent_ref.children.lookup(name, guard) {
                        DEntryChildLookup::Found(installed) => (*installed).into_ident_ref(),
                        DEntryChildLookup::Missing => return Err(Errno::ESTALE),
                    };
                    resumed
                        .trail
                        .push(TrailEntry::DEntry(resumed.cursor))
                        .map_err(|_| Errno::ENAMETOOLONG)?;
                    resumed.cursor = installed;
                    Ok(resumed)
                }
            }
        }
    }
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
fn map_children_insert_error(err: crate::vfs::structure::DEntryChildrenInstallError) -> Errno {
    match err {
        crate::vfs::structure::DEntryChildrenInstallError::AlreadyPresent => Errno::EBUSY,
        crate::vfs::structure::DEntryChildrenInstallError::Full => Errno::EBUSY,
        crate::vfs::structure::DEntryChildrenInstallError::Busy => Errno::EBUSY,
        crate::vfs::structure::DEntryChildrenInstallError::Missing => Errno::ESTALE,
    }
}
