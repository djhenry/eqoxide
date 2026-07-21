//! GPU frame → PNG encoding for the /frame HTTP endpoint.

/// Encode a mapped GPU texture buffer as PNG.
///
/// `row_pitch` is the padded row stride returned by wgpu (may be larger than
/// `width * 4` due to alignment).  BGRA format is automatically swapped to RGBA.
///
/// `max_dim`: if `Some(n)`, the longest edge is scaled down to at most `n`
/// pixels while preserving the aspect ratio (pass `None` for full resolution).
pub fn encode_frame_png(
    mapped:    &[u8],
    width:     u32,
    height:    u32,
    row_pitch: u32,
    format:    wgpu::TextureFormat,
    max_dim:   Option<u32>,
) -> Vec<u8> {
    let is_bgra = matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut rgba: Vec<u8> = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height {
        let start    = (row * row_pitch) as usize;
        let row_bytes = &mapped[start..start + (width * 4) as usize];
        if is_bgra {
            for px in row_bytes.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        } else {
            rgba.extend_from_slice(row_bytes);
        }
    }

    let (out_w, out_h, pixels) = if let Some(max) = max_dim {
        if width > max || height > max {
            let img = image::RgbaImage::from_raw(width, height, rgba)
                .expect("RGBA buffer size mismatch");
            let resized = image::DynamicImage::from(img).resize(
                max, max,
                image::imageops::FilterType::Triangle,
            );
            let r = resized.to_rgba8();
            (r.width(), r.height(), r.into_raw())
        } else {
            (width, height, rgba)
        }
    } else {
        (width, height, rgba)
    };

    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&pixels, out_w, out_h, image::ExtendedColorType::Rgba8)
    .unwrap_or_default();
    out
}
