//! A downscaled copy of a PNG screenshot too large to inline in a tool result.

/// A PNG preview of a larger screenshot, `1/factor` of its size per axis.
pub(crate) struct Preview {
    pub(crate) png: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) factor: u32,
}

/// The least whole-factor box downscale of `png` whose base64 form fits in
/// `max_encoded` bytes. `None` when the PNG cannot be decoded or no factor fits.
pub(crate) fn downscaled(png: &[u8], max_encoded: usize) -> Option<Preview> {
    let (width, height, rgb) = decode_rgb(png)?;
    let ratio = base64_len(png.len()) as f64 / max_encoded.max(1) as f64;
    let mut factor = (ratio.sqrt().ceil() as u32).max(2);
    loop {
        let out_width = (width / factor).max(1);
        let out_height = (height / factor).max(1);
        let pixels = box_downscale(&rgb, width, height, factor, out_width, out_height);
        let encoded = encode_rgb(out_width, out_height, &pixels)?;
        if base64_len(encoded.len()) <= max_encoded {
            return Some(Preview {
                png: encoded,
                width: out_width,
                height: out_height,
                factor,
            });
        }
        if out_width == 1 && out_height == 1 {
            return None;
        }
        factor += factor.div_ceil(2);
    }
}

fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// Width, height and 8-bit RGB pixels of a PNG.
fn decode_rgb(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
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

/// Each output pixel is the mean of its `factor` x `factor` source block,
/// clipped at the image edge.
fn box_downscale(
    rgb: &[u8],
    width: u32,
    height: u32,
    factor: u32,
    out_width: u32,
    out_height: u32,
) -> Vec<u8> {
    let (width, height, factor) = (width as usize, height as usize, factor as usize);
    let mut out = Vec::with_capacity(out_width as usize * out_height as usize * 3);
    for out_y in 0..out_height as usize {
        let rows = out_y * factor..((out_y + 1) * factor).min(height);
        for out_x in 0..out_width as usize {
            let columns = out_x * factor..((out_x + 1) * factor).min(width);
            let mut sum = [0u64; 3];
            for y in rows.clone() {
                for x in columns.clone() {
                    let offset = (y * width + x) * 3;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u64::from(rgb[offset + channel]);
                    }
                }
            }
            let count = (rows.len() * columns.len()).max(1) as u64;
            out.extend(sum.iter().map(|total| (total / count) as u8));
        }
    }
    out
}

fn encode_rgb(width: u32, height: u32, pixels: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(pixels).ok()?;
    writer.finish().ok()?;
    Some(bytes)
}
