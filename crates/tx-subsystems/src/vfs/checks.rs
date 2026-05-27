//! VFS checks: pure observation contexts and witness types.
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1` §checks: this module produces resolution
//! contexts and path-resolution witnesses. The witnesses are *carried*
//! into execution-side step bodies; they prove that observed state was
//! re-predicated under an epoch guard. No mutation lives here.

use crate::vfs::adapter::step_engine::{Cap, IdentRef};

use crate::mount::MountNamespace;

use super::structure::{Credential, DEntry, InlineName, RNode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootCtx {
    pub root: Cap<DEntry>,
    pub mount_ns: Option<Cap<MountNamespace>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveCtx {
    pub root: RootCtx,
    pub cwd: Cap<DEntry>,
    pub credential: Credential,
}

// --- Witness types: lifetime-parameterized, IdentRef-carried observations ---

#[derive(Debug)]
pub struct EntityAtPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub rnode: IdentRef<'g, RNode>,
}

#[derive(Debug)]
pub struct DirectoryAtPath<'g> {
    pub dentry: IdentRef<'g, DEntry>,
    pub rnode: IdentRef<'g, RNode>,
}

#[derive(Debug)]
pub struct ParentAndName<'g> {
    pub parent: IdentRef<'g, DEntry>,
    pub name: InlineName,
}

// --- Construction helpers ---

impl<'g> EntityAtPath<'g> {
    pub fn from_caps(
        dentry: &Cap<DEntry>,
        rnode: &Cap<RNode>,
        guard: &'g crate::execution::Guard<'_>,
    ) -> Self {
        Self {
            dentry: dentry.ident_ref(guard),
            rnode: rnode.ident_ref(guard),
        }
    }
}

impl<'g> DirectoryAtPath<'g> {
    pub fn from_caps(
        dentry: &Cap<DEntry>,
        rnode: &Cap<RNode>,
        guard: &'g crate::execution::Guard<'_>,
    ) -> Self {
        Self {
            dentry: dentry.ident_ref(guard),
            rnode: rnode.ident_ref(guard),
        }
    }
}

impl<'g> ParentAndName<'g> {
    pub fn from_cap(
        parent: &Cap<DEntry>,
        name: InlineName,
        guard: &'g crate::execution::Guard<'_>,
    ) -> Self {
        Self {
            parent: parent.ident_ref(guard),
            name,
        }
    }
}
