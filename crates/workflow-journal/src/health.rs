//! Shared scan facts. Decoders retain ownership of record applicability and recovery.
use std::io;
use tokio::io::AsyncBufRead;

use crate::{read_jsonl_line_buffered, JsonlLine, JsonlRead};

#[derive(Debug, Clone, Copy)]
pub enum SequenceRule {
    None,
    Increasing,
    Consecutive { first: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceVerdict {
    Unsequenced,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanControl {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerIssueKind {
    TornTail,
    OversizedLine,
    Decode,
    Schema,
    Sequence,
    SequenceExhausted,
    Checksum,
    InvalidState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerIssue {
    pub kind: LedgerIssueKind,
    pub line: Option<usize>,
    pub offset: Option<u64>,
}

/// Diagnostic observations only; record counts do not confer replay authority.
#[derive(Debug, Clone, Default)]
pub struct LedgerHealth {
    pub exists: bool,
    pub bytes_observed: u64,
    pub complete_lines: usize,
    pub accepted_records: usize,
    pub decoded_records: usize,
    pub incompatible_records: usize,
    pub max_observed_sequence: Option<u64>,
    pub last_accepted_sequence: Option<u64>,
    pub issue_counts: [usize; 8],
    pub first_issue: Option<LedgerIssue>,
}

impl LedgerHealth {
    pub fn issue_count(&self, kind: LedgerIssueKind) -> usize {
        self.issue_counts[kind as usize]
    }

    fn issue(&mut self, kind: LedgerIssueKind, line: usize, offset: u64) {
        let count = &mut self.issue_counts[kind as usize];
        *count = count.saturating_add(1);
        self.first_issue.get_or_insert(LedgerIssue {
            kind,
            line: Some(line),
            offset: Some(offset),
        });
    }
}

pub enum RecordObservation {
    Skip,
    Accepted {
        sequence: Option<u64>,
    },
    Rejected {
        sequence_hint: Option<u64>,
        kind: LedgerIssueKind,
    },
}

pub struct PreparedRecord<T> {
    pub observation: RecordObservation,
    pub record: Option<T>,
}

pub trait LedgerDecoder {
    type Record;
    /// Prepare and validate without committing authoritative domain state.
    fn decode(&mut self, line: &JsonlLine<'_>) -> io::Result<PreparedRecord<Self::Record>>;
    /// Apply only after sequence validation; invalid records may remain diagnostic.
    fn apply(
        &mut self,
        line: &JsonlLine<'_>,
        prepared: PreparedRecord<Self::Record>,
        verdict: SequenceVerdict,
    ) -> io::Result<ScanControl>;
}

pub struct ScanOptions {
    pub max_line_bytes: u64,
    pub sequence_rule: SequenceRule,
}

pub async fn scan_jsonl<R: AsyncBufRead + Unpin, D: LedgerDecoder>(
    reader: &mut R,
    options: ScanOptions,
    decoder: &mut D,
) -> io::Result<LedgerHealth> {
    scan(reader, None, options, decoder).await
}

/// Consume the already-read format-detection line without reading or parsing it twice.
pub async fn scan_jsonl_prefetched<R: AsyncBufRead + Unpin, D: LedgerDecoder>(
    reader: &mut R,
    first: JsonlRead,
    options: ScanOptions,
    decoder: &mut D,
) -> io::Result<LedgerHealth> {
    scan(reader, Some(first), options, decoder).await
}

async fn scan<R: AsyncBufRead + Unpin, D: LedgerDecoder>(
    reader: &mut R,
    mut first: Option<JsonlRead>,
    options: ScanOptions,
    decoder: &mut D,
) -> io::Result<LedgerHealth> {
    let mut health = LedgerHealth {
        exists: true,
        ..LedgerHealth::default()
    };
    let mut buffer = Vec::new();
    loop {
        let next = match first.take() {
            Some(first) => first,
            None => read_jsonl_line_buffered(reader, options.max_line_bytes, buffer).await?,
        };
        let offset = health.bytes_observed;
        let number = health.complete_lines.saturating_add(1);
        let bytes = match next {
            JsonlRead::Eof => return Ok(health),
            JsonlRead::TooLong => {
                health.bytes_observed =
                    offset.saturating_add(options.max_line_bytes.saturating_add(1));
                health.issue(LedgerIssueKind::OversizedLine, number, offset);
                return Ok(health);
            }
            JsonlRead::Torn(bytes) => {
                health.bytes_observed = offset.saturating_add(bytes.len() as u64);
                health.issue(LedgerIssueKind::TornTail, number, offset);
                return Ok(health);
            }
            JsonlRead::Complete(bytes) => bytes,
        };
        health.bytes_observed = offset.saturating_add(bytes.len() as u64);
        health.complete_lines = number;
        let line = JsonlLine {
            bytes: &bytes,
            number,
            offset,
        };
        let prepared = decoder.decode(&line)?;
        if prepared.record.is_some() {
            health.decoded_records = health.decoded_records.saturating_add(1);
        }
        let (sequence, rejected, skipped) = match prepared.observation {
            RecordObservation::Skip => (None, false, true),
            RecordObservation::Accepted { sequence } => (sequence, false, false),
            RecordObservation::Rejected {
                sequence_hint,
                kind,
            } => {
                health.issue(kind, number, offset);
                (sequence_hint, true, false)
            }
        };
        let sequence_valid = match (options.sequence_rule, sequence) {
            (SequenceRule::None, _) | (_, None) => true,
            (SequenceRule::Increasing, Some(sequence)) => health
                .max_observed_sequence
                .is_none_or(|previous| sequence > previous),
            (SequenceRule::Consecutive { first }, Some(sequence)) => {
                let expected = match health.last_accepted_sequence {
                    Some(previous) => previous.checked_add(1),
                    None => Some(first),
                };
                expected == Some(sequence)
            }
        };
        if !sequence_valid {
            health.issue(LedgerIssueKind::Sequence, number, offset);
        }
        if let Some(sequence) = sequence {
            health.max_observed_sequence = health.max_observed_sequence.max(Some(sequence));
            if sequence == u64::MAX && health.issue_count(LedgerIssueKind::SequenceExhausted) == 0 {
                health.issue(LedgerIssueKind::SequenceExhausted, number, offset);
            }
        }
        let verdict = if rejected || !sequence_valid {
            health.incompatible_records = health.incompatible_records.saturating_add(1);
            SequenceVerdict::Invalid
        } else if sequence.is_some() && !matches!(options.sequence_rule, SequenceRule::None) {
            SequenceVerdict::Valid
        } else {
            SequenceVerdict::Unsequenced
        };
        if !skipped && verdict != SequenceVerdict::Invalid {
            health.accepted_records = health.accepted_records.saturating_add(1);
            if sequence.is_some() {
                health.last_accepted_sequence = sequence;
            }
        }
        if decoder.apply(&line, prepared, verdict)? == ScanControl::Stop {
            return Ok(health);
        }
        buffer = bytes;
    }
}
