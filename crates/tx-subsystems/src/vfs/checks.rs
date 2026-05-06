//! VFS checks: pure observation contexts and witness types.
//!
//! Per `SUBSYSTEM_ANATOMY_v2_1` §checks: this module produces resolution
//! contexts and path-resolution witnesses. The witnesses are *carried*
//! into execution-side step bodies; they prove that observed state was
//! re-predicated under an epoch guard. No mutation lives here.

use tx_substrate::zone::Cap;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntityAtPath {
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryAtPath {
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParentAndName {
    pub parent: Cap<DEntry>,
    pub name: InlineName,
}
