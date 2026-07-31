//! I/O manager control-plane scaffolding.
//!
//! The staged path currently defines request, plan, queue, budget, and
//! PageService ownership values. It keeps the existing
//! `PageContainer -> FsPageBacking -> BlockDeviceOps` executor path available
//! while L4 submission and completion routing are being introduced behind it.

mod adapter;

pub mod backend;
pub mod block;
pub mod page;
pub mod runtime;
