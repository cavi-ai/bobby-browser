//! PNG size, decoding, resizing and shrinking.

/// Longest side, in pixels, of a compact screenshot. Page text stays legible
/// at this size, and an image model's prompt time grows with its pixels.
pub const COMPACT_MAX_EDGE: u32 = 1280;

/// Width and height from a PNG's leading IHDR chunk.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(..8)? != b"\x89PNG\r\n\x1a\n" || png.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// Width, height and 8-bit RGB pixels of a PNG.
pub fn decode_rgb(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
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
    Some((info.width, info.height, rgb))
}

/// Resizes 8-bit RGB pixels with a triangle filter widened by the scale
/// when shrinking, one axis at a time with rounding between passes.
pub fn resize_rgb(
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

/// `png` shrunk so its longest side is `max_edge` pixels. `None` when it
/// already fits or does not decode.
pub fn shrink_png(png: &[u8], max_edge: u32) -> Option<Vec<u8>> {
    let (width, height) = png_size(png)?;
    let longest = width.max(height);
    if max_edge == 0 || longest <= max_edge {
        return None;
    }
    let ratio = f64::from(max_edge) / f64::from(longest);
    let out_width = ((f64::from(width) * ratio).round() as u32).max(1);
    let out_height = ((f64::from(height) * ratio).round() as u32).max(1);
    let (width, height, rgb) = decode_rgb(png)?;
    let resized = resize_rgb(&rgb, width, height, out_width as usize, out_height as usize);
    let mut shrunk = Vec::new();
    let mut encoder = png::Encoder::new(&mut shrunk, out_width, out_height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&resized).ok()?;
    writer.finish().ok()?;
    Some(shrunk)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(pixels)
            .unwrap();
        bytes
    }

    #[test]
    fn rgb_rgba_and_gray_pngs_decode_to_rgb() {
        let rgba = encode(
            2,
            1,
            png::ColorType::Rgba,
            &[10, 20, 30, 255, 40, 50, 60, 0],
        );
        assert_eq!(
            decode_rgb(&rgba),
            Some((2, 1, vec![10, 20, 30, 40, 50, 60]))
        );
        let gray = encode(1, 2, png::ColorType::Grayscale, &[7, 9]);
        assert_eq!(decode_rgb(&gray), Some((1, 2, vec![7, 7, 7, 9, 9, 9])));
        assert_eq!(decode_rgb(b"GIF89a"), None);
    }

    #[test]
    fn shrinking_averages_neighbours_and_keeps_flat_color() {
        // A 4x1 row 0, 100, 200, 100 halved reads three inputs per output with the
        // triangle filter widened to two samples; Pillow's bilinear resize gives 71, 143.
        let row: Vec<u8> = [0u8, 100, 200, 100].iter().flat_map(|v| [*v; 3]).collect();
        let half = resize_rgb(&row, 4, 1, 2, 1);
        assert_eq!(half, vec![71, 71, 71, 143, 143, 143]);
        let flat = vec![77u8; 5 * 3 * 3];
        assert!(resize_rgb(&flat, 5, 3, 3, 2).iter().all(|v| *v == 77));
    }
}
