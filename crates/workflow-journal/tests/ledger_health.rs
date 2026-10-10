use std::io;
use tokio::io::BufReader;
use workflow_journal::{
    read_jsonl_line, scan_jsonl, scan_jsonl_prefetched, JsonlLine, LedgerDecoder, LedgerIssueKind,
    PreparedRecord, RecordObservation, ScanControl, ScanOptions, SequenceRule, SequenceVerdict,
};

#[derive(Default)]
struct Decoder {
    calls: Vec<(usize, u64, SequenceVerdict)>,
    applied: Vec<u64>,
    stop_on_invalid: bool,
}
impl LedgerDecoder for Decoder {
    type Record = u64;
    fn decode(&mut self, line: &JsonlLine<'_>) -> io::Result<PreparedRecord<u64>> {
        let text = std::str::from_utf8(line.bytes).unwrap().trim();
        let rejected = text.starts_with('!');
        let sequence = text.trim_start_matches('!').parse().ok();
        Ok(PreparedRecord {
            record: if rejected { None } else { sequence },
            observation: if rejected {
                RecordObservation::Rejected {
                    sequence_hint: sequence,
                    kind: LedgerIssueKind::Decode,
                }
            } else {
                RecordObservation::Accepted { sequence }
            },
        })
    }
    fn apply(
        &mut self,
        line: &JsonlLine<'_>,
        record: PreparedRecord<u64>,
        verdict: SequenceVerdict,
    ) -> io::Result<ScanControl> {
        self.calls.push((line.number, line.offset, verdict));
        if verdict != SequenceVerdict::Invalid {
            self.applied.extend(record.record);
        }
        Ok(
            if self.stop_on_invalid && verdict == SequenceVerdict::Invalid {
                ScanControl::Stop
            } else {
                ScanControl::Continue
            },
        )
    }
}
fn options(rule: SequenceRule) -> ScanOptions {
    ScanOptions {
        max_line_bytes: 64,
        sequence_rule: rule,
    }
}

#[tokio::test]
async fn increasing_sequence_is_checked_before_domain_application() {
    let mut reader = BufReader::new(&b"1\n9\n9\n3\n"[..]);
    let mut decoder = Decoder::default();
    let report = scan_jsonl(&mut reader, options(SequenceRule::Increasing), &mut decoder)
        .await
        .unwrap();
    assert_eq!(decoder.applied, [1, 9]);
    assert_eq!(
        decoder.calls.iter().map(|v| v.2).collect::<Vec<_>>(),
        [
            SequenceVerdict::Valid,
            SequenceVerdict::Valid,
            SequenceVerdict::Invalid,
            SequenceVerdict::Invalid
        ]
    );
    assert_eq!(report.max_observed_sequence, Some(9));
    assert_eq!(report.incompatible_records, 2);
    assert_eq!(report.issue_count(LedgerIssueKind::Sequence), 2);
    assert_eq!(report.decoded_records, 4);
}

#[tokio::test]
async fn consecutive_gap_does_not_advance_accepted_sequence() {
    let mut reader = BufReader::new(&b"1\n3\n"[..]);
    let mut decoder = Decoder {
        stop_on_invalid: true,
        ..Decoder::default()
    };
    let report = scan_jsonl(
        &mut reader,
        options(SequenceRule::Consecutive { first: 1 }),
        &mut decoder,
    )
    .await
    .unwrap();
    assert_eq!(decoder.applied, [1]);
    assert_eq!(report.last_accepted_sequence, Some(1));
    assert_eq!(report.issue_count(LedgerIssueKind::Sequence), 1);
}

#[tokio::test]
async fn rejected_sequence_hint_remains_high_water() {
    let mut reader = BufReader::new(&b"!9\n8\n"[..]);
    let mut decoder = Decoder::default();
    let report = scan_jsonl(&mut reader, options(SequenceRule::Increasing), &mut decoder)
        .await
        .unwrap();
    assert!(decoder.applied.is_empty());
    assert_eq!(report.max_observed_sequence, Some(9));
    assert_eq!(report.incompatible_records, 2);
}

#[tokio::test]
async fn oversized_line_stops_before_a_suffix_can_be_decoded() {
    let mut reader = BufReader::new(&b"xxxxxxxx\n1\n"[..]);
    let mut decoder = Decoder::default();
    let report = scan_jsonl(
        &mut reader,
        ScanOptions {
            max_line_bytes: 4,
            sequence_rule: SequenceRule::None,
        },
        &mut decoder,
    )
    .await
    .unwrap();
    assert!(decoder.calls.is_empty());
    assert_eq!(report.issue_count(LedgerIssueKind::OversizedLine), 1);
    assert_eq!(report.bytes_observed, 5);
}

#[tokio::test]
async fn prefetched_line_is_scanned_once_with_original_offsets() {
    let mut reader = BufReader::new(&b"1\n2\n"[..]);
    let first = read_jsonl_line(&mut reader, 64).await.unwrap();
    let mut decoder = Decoder::default();
    let report = scan_jsonl_prefetched(
        &mut reader,
        first,
        options(SequenceRule::Increasing),
        &mut decoder,
    )
    .await
    .unwrap();
    assert_eq!(
        decoder.calls,
        [
            (1, 0, SequenceVerdict::Valid),
            (2, 2, SequenceVerdict::Valid)
        ]
    );
    assert_eq!(report.bytes_observed, 4);
}

#[tokio::test]
async fn torn_tail_is_not_applied() {
    let mut reader = BufReader::new(&b"1\n2"[..]);
    let mut decoder = Decoder::default();
    let report = scan_jsonl(&mut reader, options(SequenceRule::Increasing), &mut decoder)
        .await
        .unwrap();
    assert_eq!(decoder.applied, [1]);
    assert_eq!(report.bytes_observed, 3);
    assert_eq!(report.issue_count(LedgerIssueKind::TornTail), 1);
    assert_eq!(report.first_issue.unwrap().offset, Some(2));
}

#[tokio::test]
async fn sequence_exhaustion_never_wraps() {
    let bytes = format!("{}\n0\n", u64::MAX);
    let mut reader = BufReader::new(bytes.as_bytes());
    let mut decoder = Decoder::default();
    let report = scan_jsonl(
        &mut reader,
        options(SequenceRule::Consecutive { first: u64::MAX }),
        &mut decoder,
    )
    .await
    .unwrap();
    assert_eq!(decoder.applied, [u64::MAX]);
    assert_eq!(report.issue_count(LedgerIssueKind::SequenceExhausted), 1);
    assert_eq!(report.issue_count(LedgerIssueKind::Sequence), 1);
    assert_eq!(report.incompatible_records, 1);
}

#[tokio::test]
async fn issue_counts_do_not_duplicate_incompatible_line_count() {
    let mut reader = BufReader::new(&b"9\n!8\n"[..]);
    let mut decoder = Decoder::default();
    let report = scan_jsonl(&mut reader, options(SequenceRule::Increasing), &mut decoder)
        .await
        .unwrap();
    assert_eq!(report.issue_count(LedgerIssueKind::Sequence), 1);
    assert_eq!(report.issue_count(LedgerIssueKind::Decode), 1);
    assert_eq!(report.incompatible_records, 1);
}

#[tokio::test]
async fn empty_stream_exists_but_missing_health_does_not() {
    let mut reader = BufReader::new(&b""[..]);
    let report = scan_jsonl(
        &mut reader,
        options(SequenceRule::None),
        &mut Decoder::default(),
    )
    .await
    .unwrap();
    assert!(report.exists);
    assert_eq!(report.complete_lines, 0);
    assert!(!workflow_journal::LedgerHealth::default().exists);
}
