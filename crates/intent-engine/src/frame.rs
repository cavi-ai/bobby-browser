//! Screenshot frames: PNG size, decoding and resizing, and the size cap a
//! frame meets before it leaves for a vision provider.

use crate::VisionAction;

pub use artifact_store::image::{decode_rgb, png_size, resize_rgb};

/// Longest side, in pixels, of a frame sent to a vision provider.
pub const VISION_FRAME_MAX_EDGE: u32 = artifact_store::image::COMPACT_MAX_EDGE;

/// How many captured-frame pixels one sent-frame pixel covers, per axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameScale {
    x: f64,
    y: f64,
}

impl FrameScale {
    /// `action` with its coordinates in the captured frame's pixels.
    pub(crate) fn restore(self, action: VisionAction) -> VisionAction {
        match action {
            VisionAction::Click { x, y } => VisionAction::Click {
                x: x * self.x,
                y: y * self.y,
            },
            VisionAction::ChallengeDetected {
                challenge_type,
                region,
                blocking,
            } => VisionAction::ChallengeDetected {
                challenge_type,
                region: region.map(|region| types::ChallengeRegion {
                    x: (f64::from(region.x) * self.x) as f32,
                    y: (f64::from(region.y) * self.y) as f32,
                    width: (f64::from(region.width) * self.x) as f32,
                    height: (f64::from(region.height) * self.y) as f32,
                }),
                blocking,
            },
            other => other,
        }
    }
}

/// `png` shrunk so its longest side is [`VISION_FRAME_MAX_EDGE`], with the
/// scale back to its pixels. `None` when it already fits or does not decode,
/// so the frame leaves as captured.
pub(crate) fn cap_frame(png: &[u8]) -> Option<(Vec<u8>, FrameScale)> {
    let (width, height) = png_size(png)?;
    let capped = artifact_store::image::shrink_png(png, VISION_FRAME_MAX_EDGE)?;
    let (out_width, out_height) = png_size(&capped)?;
    Some((
        capped,
        FrameScale {
            x: f64::from(width) / f64::from(out_width),
            y: f64::from(height) / f64::from(out_height),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portrait_frame_caps_its_height() {
        let mut tall = Vec::new();
        let mut encoder = png::Encoder::new(&mut tall, 700, 2800);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![200; 700 * 2800 * 3])
            .unwrap();
        let (capped, scale) = cap_frame(&tall).expect("a frame over the cap shrinks");
        assert_eq!(png_size(&capped), Some((320, 1280)));
        assert_eq!(
            scale,
            FrameScale {
                x: 700.0 / 320.0,
                y: 2800.0 / 1280.0
            }
        );
    }
}
