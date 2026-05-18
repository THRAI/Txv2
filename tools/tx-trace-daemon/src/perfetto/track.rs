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

    /// Rename an existing process track, emitting a fresh
    /// `TrackDescriptor` keyed by the same UUID so Perfetto picks up
    /// the new `ProcessDescriptor.process_name` retroactively. Returns
    /// the descriptor to emit, or `None` if the PID has no track yet
    /// (caller falls through to `ensure_process_track` at first slice
    /// time) or if `comm` matches the cached name (idempotent).
    ///
    /// Used by the writer when a `PayloadProcessLabel` arrives after
    /// the process track has already been materialised by an earlier
    /// slice — typically when `fork` + `execve` straddle the first
    /// poll of the child. Without this the track would keep the
    /// pre-execve `comm` (often the parent's name) for the rest of
    /// the trace.
    pub fn rename_process_track(
        &mut self,
        pid: u32,
        comm: &str,
    ) -> Option<TrackDescriptor> {
        let track_id = 0xF000_0000_0000_0000u64 | pid as u64;
        let key = TrackKey::KernelTrackId(track_id);
        let entry = self.map.get_mut(&key)?;
        if entry.name == comm {
            return None;
        }
        entry.name = comm.to_string();
        Some(TrackDescriptor {
            uuid: Some(entry.uuid),
            parent_uuid: None,
            name: Some(comm.to_string()),
            process: Some(ProcessDescriptor {
                pid: Some(pid as i32),
                process_name: Some(comm.to_string()),
            }),
            thread: None,
        })
    }

    /// Ensure a per-process track exists; returns UUID + optional descriptor.
    ///
    /// `pid` is the kernel PID (low 32 bits of `process.pid.0`). Process
    /// tracks are top-level (no `parent_uuid`) so Perfetto can show
    /// each process as its own swimlane with thread tracks nested
    /// underneath. `pid = 0` is the reserved "kernel actor" sentinel
    /// — callers may either skip the call or render those tracks
    /// directly under the harts swimlane.
    ///
    /// Uses a namespaced `KernelTrackId` (`0xF000_0000_0000_0000 | pid`)
    /// so process tracks never collide with the per-hart task tracks
    /// (which embed the hart in their high half) or with the harts
    /// process track at UUID 1.
    ///
    /// `comm` (when `Some`) carries the PCB short name read from
    /// `PayloadProcessLabel`; we use it as the
    /// `ProcessDescriptor.process_name` so the Perfetto track header
    /// reads e.g. `busybox` instead of `pid-12`. Track-allocation is
    /// one-shot: subsequent calls with a different `comm` are
    /// ignored (first writer wins). If the daemon sees the
    /// ProcessLabel before any slice on that PID — the kernel emits
    /// it right after submit — this is the common case and the
    /// rendered name is correct.
    pub fn ensure_process_track(
        &mut self,
        pid: u32,
        comm: Option<&str>,
    ) -> (u64, Option<TrackDescriptor>) {
        let track_id = 0xF000_0000_0000_0000u64 | pid as u64;
        let key = TrackKey::KernelTrackId(track_id);
        if let Some(e) = self.map.get(&key) {
            return (e.uuid, None);
        }
        let uuid = self.alloc_uuid();
        let synthetic = format!("pid-{pid}");
        let display_name = comm.map(|c| c.to_string()).unwrap_or_else(|| synthetic.clone());
        self.map.insert(
            key,
            TrackEntry { uuid, parent_uuid: 0, name: display_name.clone() },
        );
        let desc = TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: None,
            name: Some(display_name.clone()),
            process: Some(ProcessDescriptor {
                pid: Some(pid as i32),
                process_name: Some(display_name),
            }),
            thread: None,
        };
        (uuid, Some(desc))
    }

    /// Ensure a per-task (thread-shaped) track parented under a
    /// process track. Returns UUID + optional descriptor.
    ///
    /// Combines [`Self::ensure_process_track`] and a thread-shaped
    /// inner track keyed by `(pid, hart, tid)` so two harts running
    /// the same TID still get distinct task tracks (rare but legal
    /// during migrations). The thread descriptor carries `pid = pid`
    /// and `tid = tid` so Perfetto's slice details show real
    /// process/thread IDs instead of the synthetic `hart0[0]
    /// txKernel[1]` placeholders.
    pub fn ensure_thread_track_under_process(
        &mut self,
        pid: u32,
        hart: u16,
        tid: u32,
        comm: Option<&str>,
    ) -> (u64, Option<TrackDescriptor>, Option<TrackDescriptor>) {
        let (process_uuid, process_desc) = self.ensure_process_track(pid, comm);
        // Namespace thread tracks distinct from raw `ensure_kernel_track`
        // task tracks (which use `(hart<<32) | tid` directly) by setting
        // the top bit. Embedding `hart` keeps two harts' views of the
        // same TID distinct.
        let track_id =
            0x8000_0000_0000_0000u64 | ((hart as u64) << 32) | tid as u64;
        let key = TrackKey::KernelTrackId(track_id);
        if let Some(e) = self.map.get(&key) {
            return (e.uuid, process_desc, None);
        }
        let uuid = self.alloc_uuid();
        let name = format!("tid-{tid}");
        self.map.insert(
            key,
            TrackEntry { uuid, parent_uuid: process_uuid, name: name.clone() },
        );
        let desc = TrackDescriptor {
            uuid: Some(uuid),
            parent_uuid: Some(process_uuid),
            name: Some(name.clone()),
            thread: Some(ThreadDescriptor {
                pid: Some(pid as i32),
                tid: Some(tid as i32),
                thread_name: Some(name),
            }),
            process: None,
        };
        (uuid, process_desc, Some(desc))
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
