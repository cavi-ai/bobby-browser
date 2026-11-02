//! What a page screenshot shows, in the screenshot's pixel space with a
//! top-left origin: lines of on-screen text (Apple Vision on macOS; other
//! platforms report [`PerceptionError::Unsupported`]) and the interactable
//! controls an ONNX detector finds.

use std::path::Path;
use std::sync::Arc;

use tract_onnx::prelude::*;

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
    #[error("control detection failed: {0}")]
    Detector(String),
}

/// An interactable control the detector found in a screenshot.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedControl {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The detector's confidence, 0 to 1.
    pub score: f32,
}

/// A detected control with the on-screen text inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedControl {
    pub name: String,
    pub control: DetectedControl,
}

/// The detector's input size, height by width; screenshots are stretched to it.
const INPUT_HEIGHT: usize = 544;
const INPUT_WIDTH: usize = 1024;
/// Score at which the trained detector was measured; lower-scored boxes are dropped.
const SCORE_FLOOR: f32 = 0.6;
/// Longest name kept for a control, in characters.
const MAX_NAME_CHARS: usize = 80;

/// An RT-DETR interactable-element detector exported to ONNX with inputs
/// `pixel_values` (1x3x544x1024, RGB 0 to 1) and outputs `logits` and
/// `pred_boxes` (normalized center, width, height).
pub struct ControlDetector {
    plan: Arc<TypedRunnableModel>,
}

impl std::fmt::Debug for ControlDetector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlDetector")
            .finish_non_exhaustive()
    }
}

impl ControlDetector {
    pub fn load(path: &Path) -> Result<Self, PerceptionError> {
        let failed = |error: TractError| PerceptionError::Detector(format!("{error:#}"));
        let plan = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(failed)?
            .with_input_fact(0, f32::fact([1, 3, INPUT_HEIGHT, INPUT_WIDTH]).into())
            .map_err(failed)?
            .into_optimized()
            .map_err(failed)?
            .into_runnable()
            .map_err(failed)?;
        Ok(Self { plan })
    }

    /// Controls in `png` scoring at least [`SCORE_FLOOR`], best first.
    pub fn detect(&self, png: &[u8]) -> Result<Vec<DetectedControl>, PerceptionError> {
        let (width, height, rgb) = decode_rgb(png)?;
        let resized = resize_bilinear(&rgb, width, height, INPUT_WIDTH, INPUT_HEIGHT);
        let plane = INPUT_WIDTH * INPUT_HEIGHT;
        let mut values = vec![0f32; 3 * plane];
        for (pixel, chunk) in resized.as_chunks::<3>().0.iter().enumerate() {
            for (channel, value) in chunk.iter().enumerate() {
                values[channel * plane + pixel] = f32::from(*value) / 255.0;
            }
        }
        let failed = |error: TractError| PerceptionError::Detector(format!("{error:#}"));
        let input =
            Tensor::from_shape(&[1, 3, INPUT_HEIGHT, INPUT_WIDTH], &values).map_err(failed)?;
        let outputs = self.plan.run(tvec!(input.into())).map_err(failed)?;
        let logits = outputs[0].to_plain_array_view::<f32>().map_err(failed)?;
        let boxes = outputs[1].to_plain_array_view::<f32>().map_err(failed)?;
        let logits: Vec<f32> = logits.iter().copied().collect();
        let boxes: Vec<f32> = boxes.iter().copied().collect();
        Ok(decode_detections(&logits, &boxes, width, height))
    }
}

/// Boxes scoring at least [`SCORE_FLOOR`] from one logit and one normalized
/// center-size box per query, in pixels of a `width` by `height` image, best first.
fn decode_detections(
    logits: &[f32],
    boxes: &[f32],
    width: u32,
    height: u32,
) -> Vec<DetectedControl> {
    let (width, height) = (f64::from(width), f64::from(height));
    let mut found: Vec<DetectedControl> = logits
        .iter()
        .zip(boxes.as_chunks::<4>().0)
        .filter_map(|(logit, cxcywh)| {
            let score = 1.0 / (1.0 + (-logit).exp());
            let [cx, cy, w, h] = cxcywh.map(f64::from);
            (score >= SCORE_FLOOR).then_some(DetectedControl {
                x: (cx - w / 2.0) * width,
                y: (cy - h / 2.0) * height,
                width: w * width,
                height: h * height,
                score,
            })
        })
        .collect();
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    found
}

/// Each control named by the text lines whose centers fall inside it, top
/// to bottom then left to right; controls with no text inside are dropped.
pub fn name_controls(controls: &[DetectedControl], text: &[TextBox]) -> Vec<NamedControl> {
    controls
        .iter()
        .filter_map(|control| {
            let mut lines: Vec<&TextBox> = text
                .iter()
                .filter(|line| {
                    let (cx, cy) = (line.x + line.width / 2.0, line.y + line.height / 2.0);
                    cx >= control.x
                        && cx <= control.x + control.width
                        && cy >= control.y
                        && cy <= control.y + control.height
                })
                .collect();
            lines.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
            let name = lines
                .iter()
                .flat_map(|line| line.text.split_whitespace())
                .collect::<Vec<_>>()
                .join(" ");
            let name: String = name.chars().take(MAX_NAME_CHARS).collect();
            (!name.is_empty()).then(|| NamedControl {
                name,
                control: control.clone(),
            })
        })
        .collect()
}

/// Width, height and 8-bit RGB pixels of a PNG.
fn decode_rgb(png: &[u8]) -> Result<(u32, u32, Vec<u8>), PerceptionError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|_| PerceptionError::InvalidImage)?;
    let mut buffer = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or(PerceptionError::InvalidImage)?
    ];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|_| PerceptionError::InvalidImage)?;
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(PerceptionError::InvalidImage),
    };
    let mut rgb = Vec::with_capacity(info.width as usize * info.height as usize * 3);
    for row in buffer
        .chunks_exact(info.line_size)
        .take(info.height as usize)
    {
        for pixel in row[..info.width as usize * channels].chunks_exact(channels) {
            match channels {
                1 | 2 => rgb.extend_from_slice(&[pixel[0]; 3]),
                _ => rgb.extend_from_slice(&pixel[..3]),
            }
        }
    }
    Ok((info.width, info.height, rgb))
}

/// Resizes 8-bit RGB pixels with a triangle filter widened by the scale
/// when shrinking, one axis at a time with rounding between passes, as the
/// detector's training images were resized.
fn resize_bilinear(
    rgb: &[u8],
    width: u32,
    height: u32,
    out_width: usize,
    out_height: usize,
) -> Vec<u8> {
    let (width, height) = (width as usize, height as usize);
    let horizontal = resample_axis(rgb, width, height, out_width, true);
    resample_axis(&horizontal, out_width, height, out_height, false)
}

/// One resampling pass along x (`along_x`) or y, from `size` to `out` samples.
fn resample_axis(rgb: &[u8], width: usize, height: usize, out: usize, along_x: bool) -> Vec<u8> {
    let size = if along_x { width } else { height };
    let weights = filter_weights(size, out);
    let (out_width, out_height) = if along_x { (out, height) } else { (width, out) };
    let mut result = vec![0u8; out_width * out_height * 3];
    for y in 0..out_height {
        for x in 0..out_width {
            let (start, coefficients) = &weights[if along_x { x } else { y }];
            for channel in 0..3 {
                let sum: f64 = coefficients
                    .iter()
                    .enumerate()
                    .map(|(offset, weight)| {
                        let source = if along_x {
                            (y * width + start + offset) * 3
                        } else {
                            ((start + offset) * width + x) * 3
                        };
                        weight * f64::from(rgb[source + channel])
                    })
                    .sum();
                result[(y * out_width + x) * 3 + channel] = sum.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    result
}

/// For each output sample, the first input sample it reads and the
/// normalized triangle-filter weights of the inputs from there on.
fn filter_weights(size: usize, out: usize) -> Vec<(usize, Vec<f64>)> {
    let scale = size as f64 / out as f64;
    let filter_scale = scale.max(1.0);
    let support = filter_scale;
    (0..out)
        .map(|index| {
            let center = (index as f64 + 0.5) * scale;
            let start = ((center - support + 0.5).floor().max(0.0)) as usize;
            let end = ((center + support + 0.5).floor() as usize).min(size);
            let mut weights: Vec<f64> = (start..end)
                .map(|source| {
                    let distance = (source as f64 - center + 0.5) / filter_scale;
                    (1.0 - distance.abs()).max(0.0)
                })
                .collect();
            let total: f64 = weights.iter().sum();
            if total > 0.0 {
                weights.iter_mut().for_each(|weight| *weight /= total);
            }
            (start, weights)
        })
        .collect()
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
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
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

    fn encode(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
        bytes
    }

    fn line(text: &str, x: f64, y: f64, width: f64, height: f64) -> TextBox {
        TextBox {
            text: text.into(),
            x,
            y,
            width,
            height,
            confidence: 1.0,
        }
    }

    fn control(x: f64, y: f64, width: f64, height: f64) -> DetectedControl {
        DetectedControl {
            x,
            y,
            width,
            height,
            score: 0.9,
        }
    }

    #[test]
    fn detections_under_the_floor_are_dropped_and_the_rest_scaled_to_pixels() {
        // Logit 2.0 scores 0.88, 0.0 scores 0.5, 1.0 scores 0.73.
        let logits = [0.0, 2.0, 1.0];
        let boxes = [
            0.5, 0.5, 0.2, 0.2, //
            0.25, 0.5, 0.1, 0.2, //
            0.75, 0.25, 0.2, 0.1,
        ];
        let found = decode_detections(&logits, &boxes, 1000, 500);
        let rounded: Vec<[i64; 4]> = found
            .iter()
            .map(|c| [c.x, c.y, c.width, c.height].map(|v| v.round() as i64))
            .collect();
        assert_eq!(rounded, vec![[200, 200, 100, 100], [650, 100, 200, 50]]);
        assert!(found[0].score > found[1].score);
    }

    #[test]
    fn controls_are_named_by_the_text_inside_them_in_reading_order() {
        let text = [
            line("report", 120.0, 40.0, 50.0, 10.0),
            line("Export", 110.0, 20.0, 50.0, 10.0),
            line("Elsewhere", 400.0, 20.0, 60.0, 10.0),
        ];
        let controls = [
            control(100.0, 10.0, 100.0, 50.0),
            control(300.0, 300.0, 40.0, 40.0),
        ];
        let named = name_controls(&controls, &text);
        assert_eq!(named.len(), 1, "a control with no text inside has no name");
        assert_eq!(named[0].name, "Export report");
        assert_eq!(named[0].control, controls[0]);
    }

    #[test]
    fn rgb_rgba_and_gray_pngs_decode_to_rgb() {
        let rgba = encode(
            2,
            1,
            png::ColorType::Rgba,
            &[10, 20, 30, 255, 40, 50, 60, 0],
        );
        assert_eq!(decode_rgb(&rgba), Ok((2, 1, vec![10, 20, 30, 40, 50, 60])));
        let gray = encode(1, 2, png::ColorType::Grayscale, &[7, 9]);
        assert_eq!(decode_rgb(&gray), Ok((1, 2, vec![7, 7, 7, 9, 9, 9])));
        assert_eq!(decode_rgb(b"GIF89a"), Err(PerceptionError::InvalidImage));
    }

    #[test]
    fn shrinking_averages_neighbours_and_keeps_flat_color() {
        // A 4x1 row 0, 100, 200, 100 halved reads three inputs per output with the
        // triangle filter widened to two samples; Pillow's bilinear resize gives 71, 143.
        let row: Vec<u8> = [0u8, 100, 200, 100].iter().flat_map(|v| [*v; 3]).collect();
        let half = resize_bilinear(&row, 4, 1, 2, 1);
        assert_eq!(half, vec![71, 71, 71, 143, 143, 143]);
        let flat = vec![77u8; 5 * 3 * 3];
        assert!(resize_bilinear(&flat, 5, 3, 3, 2).iter().all(|v| *v == 77));
    }

    #[test]
    fn normalized_bottom_left_boxes_become_top_left_pixels() {
        assert_eq!(
            to_pixels([0.25, 0.75, 0.5, 0.125], 200, 80),
            [50.0, 10.0, 100.0, 10.0]
        );
    }
}
