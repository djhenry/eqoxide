//! GPU frame → PNG encoding for the /frame HTTP endpoint.

/// Encode a mapped GPU texture buffer as PNG.
///
/// `row_pitch` is the padded row stride returned by wgpu (may be larger than
/// `width * 4` due to alignment).  BGRA format is automatically swapped to RGBA.
pub fn encode_frame_png(
    mapped:    &[u8],
    width:     u32,
    height:    u32,
    row_pitch: u32,
    format:    wgpu::TextureFormat,
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

    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
    .unwrap_or_default();
    out
}
