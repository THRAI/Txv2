//! SpanTable — tracks open spans and reconstructs Perfetto slice begin/end pairs.
//!
//! Data structure: `HashMap<(hart: u16, span_id: u64), SpanEntry>`.
//! On `SpanBegin`: insert.
//! On `SpanEnd`: remove and emit begin+end TrackEvent packets.
//! On flush: emit repair instants for spans open > MAX_OPEN_SPAN_DURATION_NS.

use std::collections::HashMap;

/// Maximum allowed open-span duration in trace-time nanoseconds (60 seconds).
/// Spans open longer than this are flushed with a `txtrace.repair.unbalanced_begin`
/// repair marker per §8 of the host spec.
pub const MAX_OPEN_SPAN_DURATION_NS: u64 = 60_000_000_000;

/// State for one open span.
#[derive(Clone, Debug)]
pub struct SpanEntry {
    /// Resolved event-name iid (references the interned event names table).
    pub name_iid: u64,
    /// Trace timestamp of SpanBegin.
    pub begin_ts: u64,
    /// Perfetto track UUID this span belongs to.
    pub track_uuid: u64,
}

/// Keyed spans awaiting their matching SpanEnd.
///
/// Data structure: `HashMap<(hart, span_id), SpanEntry>` — O(1) insert/remove.
pub struct SpanTable {
    by_key: HashMap<(u16, u64), SpanEntry>,
}

/// The result of processing a SpanEnd (or a flush of stale spans).
///
/// Some fields (`name_iid`, `is_repair`, `span_id`) are read by the Perfetto
/// writer and/or tests; the `#[allow]` suppresses dead-code warnings when
/// those callers are in other modules.
pub struct ClosedSpan {
    pub begin_ts: u64,
    pub end_ts: u64,
    pub name_iid: u64,
    pub track_uuid: u64,
    /// True if the span was force-closed by the stale-span sweep (not a real End).
    pub is_repair: bool,
    /// The original span_id (for repair marker annotations).
    pub span_id: u64,
}

/// Result of processing an orphan SpanEnd (no matching begin found).
#[derive(Debug)]
pub struct OrphanEnd {
    pub ts: u64,
    pub span_id: u64,
    pub track_uuid: u64,
}

impl SpanTable {
    pub fn new() -> Self {
        Self { by_key: HashMap::new() }
    }

    /// Record a SpanBegin.  Returns `true` if the key was new (normal), `false`
    /// if it collided with an existing open span (silently overwrites — the old
    /// span was never closed, which is an anomaly the caller should handle by
    /// first calling `flush_stale`).
    pub fn begin(&mut self, hart: u16, span_id: u64, entry: SpanEntry) -> bool {
        self.by_key.insert((hart, span_id), entry).is_none()
    }

    /// Record a SpanEnd.  Returns `Ok(ClosedSpan)` if matched, `Err(OrphanEnd)`
    /// if the span was never opened (attach-late or ring overflow).
    pub fn end(
        &mut self,
        hart: u16,
        span_id: u64,
        end_ts: u64,
        track_uuid_hint: u64,
    ) -> Result<ClosedSpan, OrphanEnd> {
        if let Some(entry) = self.by_key.remove(&(hart, span_id)) {
            Ok(ClosedSpan {
                begin_ts: entry.begin_ts,
                end_ts,
                name_iid: entry.name_iid,
                track_uuid: entry.track_uuid,
                is_repair: false,
                span_id,
            })
        } else {
            Err(OrphanEnd { ts: end_ts, span_id, track_uuid: track_uuid_hint })
        }
    }

    /// Drain all spans whose `begin_ts` is more than `MAX_OPEN_SPAN_DURATION_NS`
    /// behind `now_ts`.  Returns them as repair-flagged ClosedSpan entries.
    pub fn flush_stale(&mut self, now_ts: u64) -> Vec<ClosedSpan> {
        let mut stale = Vec::new();
        self.by_key.retain(|(_, span_id), entry| {
            if now_ts.saturating_sub(entry.begin_ts) > MAX_OPEN_SPAN_DURATION_NS {
                stale.push(ClosedSpan {
                    begin_ts: entry.begin_ts,
                    end_ts: now_ts,
                    name_iid: entry.name_iid,
                    track_uuid: entry.track_uuid,
                    is_repair: true,
                    span_id: *span_id,
                });
                false
            } else {
                true
            }
        });
        stale
    }

    /// Drain *all* remaining open spans as repair entries (called at end-of-file
    /// to close any spans that were never ended before the region finished).
    pub fn flush_all(&mut self, now_ts: u64) -> Vec<ClosedSpan> {
        let entries: Vec<_> = self.by_key.drain().collect();
        entries
            .into_iter()
            .map(|((_, span_id), entry)| ClosedSpan {
                begin_ts: entry.begin_ts,
                end_ts: now_ts,
                name_iid: entry.name_iid,
                track_uuid: entry.track_uuid,
                is_repair: true,
                span_id,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64) -> SpanEntry {
        SpanEntry { name_iid: 1, begin_ts: ts, track_uuid: 42 }
    }

    #[test]
    fn begin_end_roundtrip() {
        let mut t = SpanTable::new();
        t.begin(0, 0xaabb, entry(1000));
        let r = t.end(0, 0xaabb, 2000, 42).expect("should match");
        assert_eq!(r.begin_ts, 1000);
        assert_eq!(r.end_ts, 2000);
        assert!(!r.is_repair);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn orphan_end_returns_err() {
        let mut t = SpanTable::new();
        let r = t.end(0, 0xdeadbeef, 5000, 42);
        assert!(r.is_err());
    }

    #[test]
    fn flush_stale_removes_old_spans() {
        let mut t = SpanTable::new();
        t.begin(0, 1, entry(0));
        t.begin(0, 2, entry(MAX_OPEN_SPAN_DURATION_NS + 1));
        // span 1 started at 0, now = MAX+10 → stale; span 2 started recent → keep
        let stale = t.flush_stale(MAX_OPEN_SPAN_DURATION_NS + 10);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].span_id, 1);
        assert!(stale[0].is_repair);
        assert_eq!(t.len(), 1); // span 2 still open
    }

    #[test]
    fn flush_all_drains_everything() {
        let mut t = SpanTable::new();
        t.begin(0, 1, entry(0));
        t.begin(1, 2, entry(1000));
        let all = t.flush_all(9999);
        assert_eq!(all.len(), 2);
        assert_eq!(t.len(), 0);
    }
}
