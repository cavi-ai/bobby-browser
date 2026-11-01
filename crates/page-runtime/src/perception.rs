//! On-screen text read from a page screenshot: one box per line of text,
//! in the screenshot's pixel space with a top-left origin. Apple Vision on
//! macOS; other platforms report [`PerceptionError::Unsupported`].

/// A line of text read from a screenshot.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBox {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The reader's confidence in the text, 0 to 1.
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PerceptionError {
    #[error("on-screen text reading is not available on this platform")]
    Unsupported,
    #[error("the screenshot is not a PNG")]
    InvalidImage,
    #[error("text reading failed: {0}")]
    Reader(String),
}

/// Every line of text the platform reader finds in `png`.
pub fn read_text(png: &[u8]) -> Result<Vec<TextBox>, PerceptionError> {
    let (width, height) = png_size(png).ok_or(PerceptionError::InvalidImage)?;
    #[cfg(target_os = "macos")]
    {
        apple::read(png, width, height)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (width, height);
        Err(PerceptionError::Unsupported)
    }
}

/// Width and height from a PNG's leading IHDR chunk.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(..8)? != b"\x89PNG\r\n\x1a\n" || png.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// A normalized box with a bottom-left origin, as Vision reports it, in
/// pixels with a top-left origin: `(x, y, width, height)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn to_pixels(normalized: [f64; 4], width: u32, height: u32) -> [f64; 4] {
    let [x, y, w, h] = normalized;
    let (width, height) = (f64::from(width), f64::from(height));
    [x * width, (1.0 - y - h) * height, w * width, h * height]
}

#[cfg(target_os = "macos")]
mod apple {
    use objc2::rc::Retained;
    use objc2::AllocAnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    use super::{to_pixels, PerceptionError, TextBox};

    pub(super) fn read(
        png: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<TextBox>, PerceptionError> {
        let data = NSData::with_bytes(png);
        let handler = VNImageRequestHandler::initWithData_options(
            VNImageRequestHandler::alloc(),
            &data,
            &NSDictionary::new(),
        );
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        // Page text is labels and names, not prose; correction rewrites them.
        request.setUsesLanguageCorrection(false);
        let base: Retained<VNRequest> = Retained::into_super(Retained::into_super(request.clone()));
        handler
            .performRequests_error(&NSArray::from_retained_slice(&[base]))
            .map_err(|error| PerceptionError::Reader(error.localizedDescription().to_string()))?;
        let Some(observations) = request.results() else {
            return Ok(Vec::new());
        };
        Ok(observations
            .iter()
            .filter_map(|observation| {
                let candidate = observation.topCandidates(1).firstObject()?;
                let rect = unsafe { observation.boundingBox() };
                let [x, y, box_width, box_height] = to_pixels(
                    [
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height,
                    ],
                    width,
                    height,
                );
                Some(TextBox {
                    text: candidate.string().to_string(),
                    x,
                    y,
                    width: box_width,
                    height: box_height,
                    confidence: candidate.confidence(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn the_png_header_gives_the_image_size() {
        assert_eq!(png_size(&png_header(1900, 980)), Some((1900, 980)));
        assert_eq!(png_size(b"GIF89a not a png"), None);
        assert_eq!(png_size(&png_header(0, 980)), None);
        assert_eq!(read_text(b"GIF89a"), Err(PerceptionError::InvalidImage));
    }

    #[test]
    fn normalized_bottom_left_boxes_become_top_left_pixels() {
        assert_eq!(
            to_pixels([0.25, 0.75, 0.5, 0.125], 200, 80),
            [50.0, 10.0, 100.0, 10.0]
        );
    }
}
