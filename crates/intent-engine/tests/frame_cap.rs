//! A frame leaves for the vision provider at most 1280 pixels on its longest
//! side, and coordinates in the answer come back in the captured frame's pixels.

mod common;

use base64::Engine;
use common::vision_endpoint;
use intent_engine::{
    HttpVisionAssist, StuckKind, VisionAction, VisionAssist, VisionProposeRequest,
};

/// A white PNG of this size with a dark block in its top-left quarter.
fn frame(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = vec![255_u8; (width * height * 3) as usize];
    for y in 0..height / 4 {
        for x in 0..width / 4 {
            let at = ((y * width + x) * 3) as usize;
            pixels[at..at + 3].copy_from_slice(&[20, 20, 20]);
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
    png
}

/// Width and height of the base64 PNG a request carried under `field`.
fn sent_size(request: &serde_json::Value, field: &str) -> (u32, u32) {
    let png = base64::engine::general_purpose::STANDARD
        .decode(request[field].as_str().expect("frame was sent"))
        .unwrap();
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let info = decoder.read_info().unwrap();
    (info.info().width, info.info().height)
}

fn request(intent_kind: &str, png: Vec<u8>) -> VisionProposeRequest {
    VisionProposeRequest {
        purpose: "continue".into(),
        intent_kind: intent_kind.into(),
        screenshot_png: Some(png.clone()),
        corpus_screenshot_png: Some(png),
        stuck: StuckKind::TargetMissing,
        context: None,
    }
}

fn click(action: VisionAction) -> (f64, f64) {
    let VisionAction::Click { x, y } = action else {
        panic!("a click answer stays a click: {action:?}");
    };
    (x, y)
}

fn assist(url: &str) -> HttpVisionAssist {
    HttpVisionAssist::new(url.into(), None, std::time::Duration::from_secs(10)).unwrap()
}

#[tokio::test]
async fn a_retina_frame_leaves_at_1280_pixels_and_its_click_maps_back() {
    let (url, requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.9,
        "action": {"kind": "click", "x": 640, "y": 331}
    }))
    .await;
    let assist = assist(&url);

    let proposal = assist
        .propose(request("locate", frame(3840, 1986)))
        .await
        .unwrap();
    assert_eq!(click(proposal.action), (1920.0, 993.0));

    let within = frame(1280, 662);
    let proposal = assist
        .propose(request("locate", within.clone()))
        .await
        .unwrap();
    assert_eq!(click(proposal.action), (640.0, 331.0));

    let requests = requests.lock().unwrap();
    assert_eq!(sent_size(&requests[0], "screenshotPng"), (1280, 662));
    assert_eq!(sent_size(&requests[0], "corpusScreenshotPng"), (1280, 662));
    assert_eq!(
        requests[1]["screenshotPng"],
        base64::engine::general_purpose::STANDARD.encode(&within),
        "a frame within the cap leaves as captured"
    );
}

#[tokio::test]
async fn a_detected_challenge_region_maps_back_to_the_captured_frame() {
    let (url, requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.9,
        "action": {
            "kind": "challengeDetected",
            "challenge_type": "recaptchaV2Checkbox",
            "region": {"x": 100, "y": 50, "width": 200, "height": 100},
            "blocking": true
        }
    }))
    .await;

    let proposal = assist(&url)
        .propose(request("detectChallenge", frame(3840, 1986)))
        .await
        .unwrap();

    let VisionAction::ChallengeDetected { region, .. } = proposal.action else {
        panic!(
            "a detection answer stays a detection: {:?}",
            proposal.action
        );
    };
    assert_eq!(
        region,
        Some(types::ChallengeRegion {
            x: 300.0,
            y: 150.0,
            width: 600.0,
            height: 300.0
        })
    );
    assert_eq!(
        sent_size(&requests.lock().unwrap()[0], "screenshotPng"),
        (1280, 662)
    );
}
