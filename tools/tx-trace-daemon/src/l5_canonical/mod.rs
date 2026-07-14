//! L5 canonical host event representation.
//!
//! This layer decodes raw txtrace record bytes into the daemon's canonical
//! event stream. L6 exporters consume these types instead of re-decoding the
//! wire ABI.

use crate::l4_readers::{RawRecordFrame, TraceIntegrity};

pub mod decode;

/// Batch transferred from L4 readers into the canonical decoder.
#[derive(Clone, Debug)]
pub struct DecodeBatch {
    frames: Vec<RawRecordFrame>,
    integrity: TraceIntegrity,
}

impl DecodeBatch {
    pub fn new(frames: Vec<RawRecordFrame>, integrity: TraceIntegrity) -> Self {
        Self { frames, integrity }
    }
}

/// Canonical decoded stream consumed by L6 projections.
#[derive(Clone, Debug)]
pub struct TraceEventStream {
    events: Vec<decode::DecodedEvent>,
    integrity: TraceIntegrity,
    markers: Vec<TraceStreamMarker>,
}

impl TraceEventStream {
    pub fn new(events: Vec<decode::DecodedEvent>, integrity: TraceIntegrity) -> Self {
        let markers = TraceStreamMarker::from_events_and_integrity(&events, &integrity);
        Self {
            events,
            integrity,
            markers,
        }
    }

    pub fn events(&self) -> &[decode::DecodedEvent] {
        &self.events
    }

    pub fn integrity(&self) -> &TraceIntegrity {
        &self.integrity
    }

    pub fn markers(&self) -> &[TraceStreamMarker] {
        &self.markers
    }
}

/// Canonical L5 metadata marker derived from decoded repairs and L4 integrity.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind")]
pub enum TraceStreamMarker {
    Repair {
        category: &'static str,
        hart: u16,
        seq_around: u64,
        details: String,
    },
    CaptureLoss {
        lost_records: u64,
        overwritten_records: u64,
    },
}

impl TraceStreamMarker {
    fn from_events_and_integrity(
        events: &[decode::DecodedEvent],
        integrity: &TraceIntegrity,
    ) -> Vec<Self> {
        let mut markers = Vec::new();
        for event in events {
            if let decode::DecodedEvent::Repair(repair) = event {
                markers.push(Self::Repair {
                    category: repair.category,
                    hart: repair.hart,
                    seq_around: repair.seq_around,
                    details: repair.details.clone(),
                });
            }
        }
        if integrity.lost_records != 0 || integrity.overwritten_records != 0 {
            markers.push(Self::CaptureLoss {
                lost_records: integrity.lost_records,
                overwritten_records: integrity.overwritten_records,
            });
        }
        markers
    }
}

/// Canonical decode contract between L4 and L5.
pub trait TraceDecoder {
    fn decode(&mut self, batch: DecodeBatch) -> Result<TraceEventStream, String>;
}

#[derive(Default)]
pub struct CanonicalDecoder;

impl TraceDecoder for CanonicalDecoder {
    fn decode(&mut self, batch: DecodeBatch) -> Result<TraceEventStream, String> {
        let events = batch.frames.iter().map(decode::decode_frame).collect();
        Ok(TraceEventStream::new(events, batch.integrity))
    }
}

#[cfg(test)]
mod tests {
    use crate::l4_readers::{TraceInputKind, TraceIntegrity};
    use crate::l5_canonical::decode::{DecodedEvent, RepairRecord};
    use crate::l5_canonical::{TraceEventStream, TraceStreamMarker};

    #[test]
    fn stream_derives_repair_and_loss_markers() {
        let integrity = TraceIntegrity {
            input_kind: TraceInputKind::TxTraceRegion,
            complete: false,
            drained_records: 4,
            retained_records: 3,
            lost_records: 2,
            overwritten_records: 1,
            repair_count: 1,
        };
        let stream = TraceEventStream::new(
            vec![DecodedEvent::Repair(RepairRecord::bad_magic(0, 7, 0xdead))],
            integrity,
        );

        assert!(matches!(
            &stream.markers()[0],
            TraceStreamMarker::Repair {
                category: "txtrace.repair.bad_magic",
                hart: 0,
                seq_around: 7,
                ..
            }
        ));
        assert_eq!(
            stream.markers()[1],
            TraceStreamMarker::CaptureLoss {
                lost_records: 2,
                overwritten_records: 1,
            }
        );
    }
}
