//! TrackRegistry — allocates Perfetto track UUIDs and synthesizes
//! TrackDescriptor packets for hart and task tracks.
//!
//! Data structure: `HashMap<TrackKey, TrackEntry>` where TrackKey is either a
//! hart index or a (kernel) track_id, and TrackEntry holds the allocated UUID
//! plus the emitted-flag.

use std::collections::HashMap;

use crate::perfetto::proto::{
    ProcessDescriptor, ThreadDescriptor, TrackDescriptor,
};

/// Discriminant for different kinds of kernel tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackKey {
    /// A hart (hardware thread) — always-on per §7.
    Hart(u16),
    /// A kernel-assigned track_id from a TrackDescriptor record.
    KernelTrackId(u64),
}

/// State for one allocated Perfetto track.
///
/// `parent_uuid` and `name` are recorded for completeness and future queries
/// (OBS-9 will use them for OnBehalfOf / DelegateEndpoint parenting).
#[derive(Clone, Debug)]
pub struct TrackEntry {
    /// Assigned Perfetto UUID (non-zero, unique within this run).
    pub uuid: u64,
    /// Parent UUID (0 = top-level).
    pub parent_uuid: u64,
    /// Human-readable name.
    pub name: String,
}

/// Registry: maps kernel track keys → Perfetto UUIDs.
///
/// Data structure: `HashMap<TrackKey, TrackEntry>` plus a monotone UUID counter.
pub struct TrackRegistry {
    map: HashMap<TrackKey, TrackEntry>,
    next_uuid: u64,
    /// UUID of the synthetic "kernel harts" process track (parent of all hart threads).
    pub harts_process_uuid: u64,
}

impl TrackRegistry {
    pub fn new() -> Self {
        // UUID 1 is reserved for the harts process track.
        Self {
            map: HashMap::new(),
            next_uuid: 2,
            harts_process_uuid: 1,
        }
    }

    fn alloc_uuid(&mut self) -> u64 {
        let u = self.next_uuid;
        self.next_uuid += 1;
        u
    }

    /// Ensure a hart track exists; returns its UUID and a TrackDescriptor packet
    /// (None if already registered).
    pub fn ensure_hart(&mut self, hart: u16) -> (u64, Option<TrackDescriptor>) {
        let key = TrackKey::Hart(hart);
        if let Some(e) = self.map.get(&key) {
            return (e.uuid, None);
        }
        let uuid = self.alloc_uuid();
        let name = format!("hart-{hart}");
        self.map.insert(
            key,
            TrackEntry { uuid, parent_uuid: self.harts_process_uuid, name: name.clone() },
        );
        let desc = TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: Some(self.harts_process_uuid),
            name: Some(name.clone()),
            thread: Some(ThreadDescriptor {
                pid: Some(1), // synthetic pid for the "kernel harts" process
                tid: Some(hart as i32),
                thread_name: Some(name),
            }),
            process: None,
        };
        (uuid, Some(desc))
    }

    /// Ensure a kernel-assigned track exists; returns UUID and optional descriptor.
    ///
    /// `track_id` is from `PayloadTrackDescriptor.track_id`.
    /// `name_str` is the resolved human name (or hex fallback).
    /// `track_kind` is the raw kind byte (0=Hart, 1=Task, 2=Process, …).
    /// `parent_hart` is the hart this track logically belongs to (for parenting).
    pub fn ensure_kernel_track(
        &mut self,
        track_id: u64,
        name_str: String,
        track_kind: u8,
        parent_hart: Option<u16>,
    ) -> (u64, Option<TrackDescriptor>) {
        let key = TrackKey::KernelTrackId(track_id);
        if let Some(e) = self.map.get(&key) {
            return (e.uuid, None);
        }
        let uuid = self.alloc_uuid();

        // Determine parent UUID.
        let parent_uuid = if let Some(h) = parent_hart {
            let (hart_uuid, _) = self.ensure_hart(h);
            hart_uuid
        } else {
            0 // top-level
        };

        self.map.insert(
            key,
            TrackEntry { uuid, parent_uuid, name: name_str.clone() },
        );

        let desc = build_descriptor(uuid, parent_uuid, name_str, track_kind);
        (uuid, Some(desc))
    }

    /// Look up a hart UUID without creating a new entry.
    /// Used by OBS-9+ when building parent-track relationships.
    #[cfg(test)]
    pub fn hart_uuid(&self, hart: u16) -> Option<u64> {
        self.map.get(&TrackKey::Hart(hart)).map(|e| e.uuid)
    }

    /// Build the "harts process" TrackDescriptor — call once at trace start.
    pub fn harts_process_descriptor(&self) -> TrackDescriptor {
        TrackDescriptor {
            uuid: Some(self.harts_process_uuid),
            parent_uuid: None,
            name: Some("txKernel harts".to_string()),
            process: Some(ProcessDescriptor {
                pid: Some(1),
                process_name: Some("txKernel".to_string()),
            }),
            thread: None,
        }
    }
}

fn build_descriptor(
    uuid: u64,
    parent_uuid: u64,
    name: String,
    track_kind: u8,
) -> TrackDescriptor {
    let parent = if parent_uuid != 0 { Some(parent_uuid) } else { None };
    match track_kind {
        // Hart (kind=0) — thread-shaped track.
        0 => TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: parent,
            name: Some(name.clone()),
            thread: Some(ThreadDescriptor {
                pid: Some(1),
                tid: None,
                thread_name: Some(name),
            }),
            process: None,
        },
        // Process (kind=2) — process-shaped track.
        2 => TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: parent,
            name: Some(name.clone()),
            process: Some(ProcessDescriptor {
                pid: None,
                process_name: Some(name),
            }),
            thread: None,
        },
        // Task (kind=1), Scope (kind=3), Endpoint (kind=4), Timer (kind=5) —
        // generic named track.
        _ => TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: parent,
            name: Some(name),
            thread: None,
            process: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hart_track_allocates_once() {
        let mut reg = TrackRegistry::new();
        let (uuid0, desc0) = reg.ensure_hart(0);
        let (uuid0b, desc0b) = reg.ensure_hart(0);
        assert_eq!(uuid0, uuid0b);
        assert!(desc0.is_some());
        assert!(desc0b.is_none(), "second call must not re-emit descriptor");

        let (uuid1, desc1) = reg.ensure_hart(1);
        assert_ne!(uuid0, uuid1);
        assert!(desc1.is_some());
    }

    #[test]
    fn kernel_track_allocates_once() {
        let mut reg = TrackRegistry::new();
        let (u0, d0) = reg.ensure_kernel_track(0xaabb, "task-0".into(), 1, Some(0));
        let (u0b, d0b) = reg.ensure_kernel_track(0xaabb, "task-0".into(), 1, Some(0));
        assert_eq!(u0, u0b);
        assert!(d0.is_some());
        assert!(d0b.is_none());
        let _ = u0; // suppress unused warning
    }
}
