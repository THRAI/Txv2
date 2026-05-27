//! Interned event-name table — maps EventNameId (u32) → Perfetto iid (u64).
//!
//! On first sight of a name_id, assigns a fresh iid and emits an
//! `InternedData.event_names` entry with the human-readable name.
//! Subsequent TrackEvents reference the iid via `name_iid`.
//!
//! Name resolution precedence (§6 of the host spec):
//!   1. names.json (--names <path>)
//!   2. Fallback: hex string "name_0x<hex>"

use std::collections::HashMap;

/// Maps EventNameId → iid for Perfetto interning.
pub struct InternTable {
    /// name_id → (iid, resolved_name_string)
    map: HashMap<u32, (u64, String)>,
    /// Monotone iid counter.
    next_iid: u64,
    /// Optional external name map loaded from names.json.
    external: HashMap<u32, String>,
}

impl InternTable {
    pub fn new() -> Self {
        Self { map: HashMap::new(), next_iid: 1, external: HashMap::new() }
    }

    /// Load an external name map (names.json contents, name_table section).
    pub fn load_external(&mut self, table: HashMap<u32, String>) {
        self.external = table;
    }

    /// Ensure `name_id` is interned.  Returns `(iid, Option<name_string>)`.
    ///
    /// If the name is new, returns `Some(name_string)` so the caller can
    /// emit an `InternedData.event_names` entry.  If already interned,
    /// returns `(existing_iid, None)`.
    pub fn intern(&mut self, name_id: u32) -> (u64, Option<String>) {
        if let Some((iid, _)) = self.map.get(&name_id) {
            return (*iid, None);
        }
        let iid = self.next_iid;
        self.next_iid += 1;
        let name = self
            .external
            .get(&name_id)
            .cloned()
            .unwrap_or_else(|| format!("name_0x{name_id:08x}"));
        self.map.insert(name_id, (iid, name.clone()));
        (iid, Some(name))
    }

    /// Return the iid if already interned, without assigning a new one.
    pub fn get_iid(&self, name_id: u32) -> Option<u64> {
        self.map.get(&name_id).map(|(iid, _)| *iid)
    }

    /// Resolve `name_id` to its registered name string, consulting the
    /// in-memory intern map first and then the external `names.json`
    /// table. Returns `None` when the id has neither been interned
    /// yet nor pre-registered — callers should fall back to the
    /// `name_0x<hex>` synthetic form.
    ///
    /// Used by the writer when it needs a *readable* base name to
    /// compose a formatted label (e.g. `a0=0x1234`) without mutating
    /// the intern table — the actual interning of the composite label
    /// happens separately via [`Self::intern_str`].
    pub fn get_resolved_name(&self, name_id: u32) -> Option<String> {
        if let Some((_, name)) = self.map.get(&name_id) {
            return Some(name.clone());
        }
        self.external.get(&name_id).cloned()
    }

    /// Intern an ASCII literal string (e.g. `"value"`) by hashing it with
    /// FNV-1a 32, the same scheme the kernel uses for stable
    /// `EventNameId`s. Returns `(iid, Some(name))` on first call and
    /// `(iid, None)` on subsequent calls — matches `intern`'s shape.
    ///
    /// Used by the writer when it needs to surface a debug-annotation
    /// field name that wasn't pre-allocated by the kernel (e.g.,
    /// `"value"` on `ArgValue` instants).
    pub fn intern_str(&mut self, s: &str) -> (u64, Option<String>) {
        let name_id = fnv1a32(s.as_bytes());
        // Mirror `intern` but use the literal `s` as the registered name
        // even if no external table entry exists — the `name_0x…`
        // fallback would discard the readable form we already have on
        // hand here.
        if let Some((iid, _)) = self.map.get(&name_id) {
            return (*iid, None);
        }
        let iid = self.next_iid;
        self.next_iid += 1;
        let name = s.to_string();
        self.map.insert(name_id, (iid, name.clone()));
        (iid, Some(name))
    }
}

/// FNV-1a 32 — matches `tx_observe::fnv1a32` exactly.
#[inline]
fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_intern_returns_new_name() {
        let mut t = InternTable::new();
        let (iid, name) = t.intern(0xabc);
        assert_eq!(iid, 1);
        assert_eq!(name, Some("name_0x00000abc".to_string()));
    }

    #[test]
    fn second_intern_returns_none_name() {
        let mut t = InternTable::new();
        let (iid0, _) = t.intern(0xabc);
        let (iid1, name1) = t.intern(0xabc);
        assert_eq!(iid0, iid1);
        assert_eq!(name1, None);
    }

    #[test]
    fn external_name_resolved() {
        let mut t = InternTable::new();
        t.load_external([(42u32, "my::cool::Op".to_string())].into());
        let (_, name) = t.intern(42);
        assert_eq!(name, Some("my::cool::Op".to_string()));
    }

    #[test]
    fn unique_iids_per_name_id() {
        let mut t = InternTable::new();
        let (a, _) = t.intern(1);
        let (b, _) = t.intern(2);
        let (c, _) = t.intern(3);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }
}
