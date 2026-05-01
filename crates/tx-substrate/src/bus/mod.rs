//! Minimal bus publication primitives.
//!
//! This first slice intentionally keeps the bus semantic-free: wires store
//! subscriber interests and invoke reactor-owned wakers, but they do not
//! evaluate readiness predicates or schedule tasks.

mod common;
mod graph;
mod macros;
mod owner;
mod port;
mod queue;
mod trace;

pub use crate::{
    bus_event_set, bus_lifecycle, bus_readiness, bus_tracepoint, bus_wire_owner_manifest,
};
pub use common::{
    DeclaredSubscriptionError, DeclaredWireError, RawSubscriptionError, RawSubscriptionState,
    RawWireError, WireDeclaration, WireDeclarationError, WireEventSet, WireKind, WireRetirement,
};
pub use graph::{
    DeclaredSubscriptionGraphKey, SubscriptionGraph, SubscriptionGraphError, SubscriptionGraphKey,
    SubscriptionGraphReady,
};
pub use owner::{
    retire_wire_owner, WireOwnerManifest, WireOwnerReclaimError, WireOwnerReclamation,
    WireOwnerRetireFence,
};
pub use port::{
    DeclaredPort, DeclaredPortSubscription, RawPort, RawPortSubscription, StaticRawPort,
};
pub use queue::{
    DeclaredQueue, DeclaredQueueSubscription, RawQueue, RawQueueSubscription, StaticRawQueue,
};
pub use trace::{RawTrace, TraceDeclaration, TracePayload};
