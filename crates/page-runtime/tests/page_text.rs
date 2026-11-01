//! Opt-in: on-screen text read from real page screenshots and scored against
//! the DOM's visible text. Set BOBBY_PERCEPTION_ROWS to rows written by the
//! ignored `perception_ground_truth` worker test.

use base64::Engine;
use page_runtime::perception::TextBox;
use page_runtime::PageRuntime;
use types::{AccessibilityNode, PageId};

/// A DOM text run with a letter or digit counts as read when a line contains its text and the run's
/// center lies inside that line's box.
fn reads(boxes: &[TextBox], run: &serde_json::Value, dpr: f64) -> bool {
    let text = run["text"].as_str().unwrap_or_default().trim();
    let bbox = &run["bbox"];
    let center_x = (bbox["x"].as_f64().unwrap() + bbox["w"].as_f64().unwrap() / 2.0) * dpr;
    let center_y = (bbox["y"].as_f64().unwrap() + bbox["h"].as_f64().unwrap() / 2.0) * dpr;
    !text.is_empty()
        && boxes.iter().any(|line| {
            line.text.contains(text)
                && (line.x..=line.x + line.width).contains(&center_x)
                && (line.y..=line.y + line.height).contains(&center_y)
        })
}

#[tokio::test]
#[ignore = "requires macOS and BOBBY_PERCEPTION_ROWS"]
async fn screen_text_reads_the_doms_visible_text_once_per_screenshot() {
    let path = std::env::var("BOBBY_PERCEPTION_ROWS")
        .expect("set BOBBY_PERCEPTION_ROWS to perception_ground_truth output");
    let runtime = PageRuntime::default();
    let (mut runs, mut read) = (0_usize, 0_usize);
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(row["imageB64"].as_str().unwrap())
            .unwrap();
        let dpr = row["viewport"]["dpr"].as_f64().unwrap_or(1.0);
        let page = PageId::new();
        runtime
            .context()
            .record(&page, vec![AccessibilityNode::default()]);

        let boxes = runtime.read_page_text(&page, png.clone()).await.unwrap();
        let again = runtime.read_page_text(&page, png).await.unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&boxes, &again),
            "{}: the same screenshot was read twice",
            row["page"]
        );

        // Glyph-only runs are icons, not text.
        let page_runs = row["textRuns"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|run| {
                run["text"]
                    .as_str()
                    .is_some_and(|text| text.chars().any(char::is_alphanumeric))
            })
            .collect::<Vec<_>>();
        let page_read = page_runs
            .iter()
            .filter(|run| reads(&boxes, run, dpr))
            .count();
        eprintln!(
            "{}: {page_read}/{} text runs read, {} lines",
            row["page"],
            page_runs.len(),
            boxes.len()
        );
        runs += page_runs.len();
        read += page_read;
    }
    let recall = read as f64 / runs as f64;
    eprintln!("recall {read}/{runs} = {recall:.3}");
    assert!(runs > 0, "no text runs in the rows");
    assert!(recall >= 0.9, "recall {recall:.3} below 0.9");
}
