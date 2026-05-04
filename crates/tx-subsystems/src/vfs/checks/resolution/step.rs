use tx_substrate::epoch::Guard;

use alloc::boxed::Box;

use crate::vfs::checks::predicates;
use crate::vfs::checks::resolution::error::WalkCause;
#[cfg(any(test, feature = "vfs-read-test-support"))]
use crate::vfs::checks::resolution::state::ResumeToken;
use crate::vfs::checks::resolution::state::{FinalSymlinkPolicy, TrailEntry, WalkState};
use crate::vfs::structure::{DEntryChildLookup, NameOwned};

pub(crate) enum KernelStep<'a, 'g> {
    Continue(Box<WalkState<'a, 'g>>),
    #[cfg(any(test, feature = "vfs-read-test-support"))]
    NeedIO(Box<IORequest>, Box<ResumeToken>),
    #[cfg(not(any(test, feature = "vfs-read-test-support")))]
    NeedIO,
    Error(WalkCause),
}

#[cfg(any(test, feature = "vfs-read-test-support"))]
pub(crate) enum IORequest {
    LookupChild { _parent_name: NameOwned },
}

pub(crate) fn kernel_step<'a, 'g>(
    mut state: WalkState<'a, 'g>,
    _policy: FinalSymlinkPolicy,
    guard: &'g Guard<'_>,
) -> KernelStep<'a, 'g> {
    if !predicates::namespace_live(&state.cursor, &state.root_ctx) {
        return KernelStep::Error(WalkCause::DetachedNamespace);
    }

    let component = match state.remaining.next_component() {
        Ok(Some(component)) => component,
        Ok(None) => return KernelStep::Error(WalkCause::MissingComponent),
        Err(_) => return KernelStep::Error(WalkCause::NameTooLong),
    };

    if component == b"." {
        return KernelStep::Continue(Box::new(state));
    }

    if component == b".." {
        if let Some(entry) = state.trail.pop() {
            match entry {
                TrailEntry::DEntry(dentry) => {
                    state.cursor = dentry;
                }
                TrailEntry::MountBoundary {
                    was_at,
                    was_in_mount,
                } => {
                    state.cursor = was_at;
                    state.current_mount = was_in_mount;
                }
            }
        }
        return KernelStep::Continue(Box::new(state));
    }

    let name = match NameOwned::from_component(component) {
        Ok(name) => name,
        Err(_) => return KernelStep::Error(WalkCause::NameTooLong),
    };

    match state.cursor.children.lookup(&name, guard) {
        DEntryChildLookup::Found(child) => {
            let child_ref = (*child).into_ident_ref();
            if !predicates::namespace_live(&child_ref, &state.root_ctx) {
                return KernelStep::Error(WalkCause::DetachedNamespace);
            }

            let child_rnode = child_ref.rnode.ident_ref(guard);
            if state.remaining.count() > 0 && !predicates::is_directory(&child_rnode) {
                return KernelStep::Error(WalkCause::NonDirectoryIntermediate);
            }

            // RFX-VFS-P2-006: After MOUNT checks/topology are ready, call
            // lookup_mount_at(child, state.root_ctx.mnt_ns, guard) here, push a
            // MountBoundary, substitute cursor to traversal.root_dentry, and
            // update current_mount before continuing.
            if state.trail.push(TrailEntry::DEntry(state.cursor)).is_err() {
                return KernelStep::Error(WalkCause::NameTooLong);
            }
            state.cursor = child_ref;
            KernelStep::Continue(Box::new(state))
        }
        DEntryChildLookup::Missing => {
            #[cfg(not(any(test, feature = "vfs-read-test-support")))]
            {
                let _ = name;
                KernelStep::NeedIO
            }

            #[cfg(any(test, feature = "vfs-read-test-support"))]
            {
                let suspended = match state.suspend() {
                    Ok(suspended) => suspended,
                    Err(_) => return KernelStep::Error(WalkCause::StaleObservation),
                };
                let parent = match state.cursor.to_cap() {
                    Ok(parent) => parent,
                    Err(_) => return KernelStep::Error(WalkCause::StaleObservation),
                };

                KernelStep::NeedIO(
                    Box::new(IORequest::LookupChild {
                        _parent_name: name.clone(),
                    }),
                    Box::new(ResumeToken::LookupChild {
                        suspended,
                        parent,
                        name,
                    }),
                )
            }
        }
    }
}
