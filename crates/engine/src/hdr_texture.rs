//! An HDR view's texture for an HDR display: the linear float render of an HDR edit (SDR white
//! = 1) as `Rgba16Float` texels, sRGB-encoded and continued above 1, as the frontend's shader
//! decodes them before scaling to the display's paper white.

/// sRGB's encoding curve, continued above 1 (egui's shader decodes it the same way).
fn srgb_encode(v: f32) -> f32 {
    if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// One linear-light pixel (SDR white = 1) for the loupe's HDR texture: negatives and non-numbers
/// to 0, brighter than `headroom` scaled down to it keeping its hue, then sRGB-encoded (the
/// shader decodes it and scales it to the display's paper white).
pub fn encode_pixel(rgb: [f32; 3], headroom: f32) -> [f32; 4] {
    let h = if headroom.is_finite() { headroom.max(1.0) } else { 1.0 };
    let c = rgb.map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 });
    let m = c[0].max(c[1]).max(c[2]);
    let k = if m > h { h / m } else { 1.0 };
    let [r, g, b] = c.map(|v| srgb_encode(v * k));
    [r, g, b, 1.0]
}

/// The largest HDR texture side accepted (the loupe's whole-frame render is far smaller).
const MAX_SIDE: usize = 16_384;

/// `rgb` (3 linear floats per pixel, `width × height`) as `Rgba16Float` texels (the bits of each half float).
pub fn encode_rgba16f(rgb: &[f32], width: usize, height: usize, headroom: f32) -> Result<Vec<u16>, String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("HDR texture size {width}×{height} is out of range"));
    }
    let n = width.checked_mul(height).ok_or("HDR texture too large")?;
    let px = rgb.as_chunks::<3>().0;
    let px = px.get(..n).ok_or("HDR render has fewer pixels than its size")?;
    let mut texels = vec![[0u16; 4]; n];
    // Full-quality previews contain millions of pixels. Reuse the raster worker pool
    // instead of evaluating three transfer curves per pixel serially on the UI thread.
    // Each pixel uses the same encoder, including display headroom and hostile inputs.
    lcc_raster::par_rows(&mut texels, width, |y, row| {
        for (out, rgb) in row.iter_mut().zip(px.get(y * width..).unwrap_or_default()) {
            *out = encode_pixel(*rgb, headroom).map(|v| half::f16::from_f32(v).to_bits());
        }
    });
    Ok(texels.into_flattened())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_go_above_sdr_white_up_to_the_headroom_keeping_their_hue() {
        // 2× SDR white with 4× headroom: encoded above 1, decodes back to 2
        let [r, ..] = encode_pixel([2.0; 3], 4.0);
        assert!(r > 1.0);
        let decoded = ((r + 0.055) / 1.055).powf(2.4);
        assert!((decoded - 2.0).abs() < 1e-4);
        // past the headroom: scaled down to it, channel ratios kept
        let [r, g, ..] = encode_pixel([8.0, 4.0, 0.0], 2.0);
        let dec = |v: f32| ((v + 0.055) / 1.055).powf(2.4);
        assert!((dec(r) - 2.0).abs() < 1e-4 && (dec(g) - 1.0).abs() < 1e-4);
        // hostile values: no NaN, no negatives
        let p = encode_pixel([f32::NAN, -3.0, f32::INFINITY], f32::NAN);
        assert!(p.iter().all(|v| v.is_finite() && *v >= 0.0));
    }

    #[test]
    fn texture_encoding_rejects_bad_sizes() {
        assert!(encode_rgba16f(&[0.5; 12], 2, 2, 4.0).is_ok_and(|t| t.len() == 16));
        assert!(encode_rgba16f(&[0.5; 9], 2, 2, 4.0).is_err(), "short buffer");
        assert!(encode_rgba16f(&[], 0, 0, 4.0).is_err());
        assert!(encode_rgba16f(&[], usize::MAX, 2, 4.0).is_err());
    }

    #[test]
    fn parallel_texture_encoding_matches_the_scalar_encoder_exactly() {
        let rgb: Vec<f32> = (0..257 * 129 * 3)
            .map(|i| match i % 29 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => -1.0,
                _ => (i % 8192) as f32 / 512.0,
            })
            .collect();
        for headroom in [1.0, 2.5, 8.0, f32::NAN] {
            let expected: Vec<u16> =
                rgb.as_chunks::<3>().0.iter().flat_map(|p| encode_pixel(*p, headroom).map(|v| half::f16::from_f32(v).to_bits())).collect();
            assert_eq!(encode_rgba16f(&rgb, 257, 129, headroom).unwrap(), expected);
        }
    }
}
