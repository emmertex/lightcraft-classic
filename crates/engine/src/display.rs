//! The monitor profile (`app.displayProfile`): previews shown on a wide-gamut or calibrated
//! display through its ICC profile.
//!
//! The frontend holds the profile in use (Settings ▸ Display, the UI command `app.displayProfile`).
//! With a profile, the loupe renders into the display's own primaries
//! ([`RenderJob::with_display`](crate::media::RenderJob::with_display)) and everything else made
//! for sRGB (thumbnails, cached previews, embedded camera JPEGs) is converted to it
//! ([`present`]). Histograms and the preview caches stay sRGB, and exports never use it.

pub use lcc_codecs::display::DisplayKind;
use lcc_codecs::display::{DisplayProfile, MAX_PROFILE_BYTES};
use lcc_pipeline::DisplaySpace;
use lcc_raster::{Coverage, Histogram, Rgba8};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::media::RenderResult;

/// A loaded display profile: its transforms and the pipeline target.
#[derive(Debug)]
pub struct Display {
    /// The file it came from.
    pub path: String,
    pub profile: DisplayProfile,
    pub space: DisplaySpace,
}

impl Display {
    /// Load the ICC profile at `path`.
    pub fn load(path: &str) -> Result<Display, String> {
        let path = path.trim();
        let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
        if meta.len() > MAX_PROFILE_BYTES as u64 {
            return Err(format!("{path}: too large for an ICC profile"));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        Display::from_bytes(path, &bytes)
    }

    pub fn from_bytes(path: &str, bytes: &[u8]) -> Result<Display, String> {
        let profile = DisplayProfile::from_icc(bytes).map_err(|e| format!("{path}: {e}"))?;
        let space = DisplaySpace::new(profile.to_rec2020, profile.id).ok_or_else(|| format!("{path}: the profile's primaries are degenerate"))?;
        Ok(Display { path: path.to_string(), profile, space })
    }

    pub fn id(&self) -> u64 {
        self.profile.id
    }

    /// Load the profile at `path` (`""`: none, the display is treated as sRGB).
    pub fn load_opt(path: &str) -> Result<Option<Arc<Display>>, String> {
        if path.trim().is_empty() { Ok(None) } else { Display::load(path).map(|d| Some(Arc::new(d))) }
    }

    /// For `app.displayProfile` and Settings: path, description, kind, primaries (xy).
    pub fn describe(&self) -> Value {
        let to_xyz = lcc_color::REC2020.to_xyz().mul(&self.profile.to_rec2020).0;
        let xy = |c: usize| {
            let (x, y, z) = (to_xyz[0][c], to_xyz[1][c], to_xyz[2][c]);
            let s = x + y + z;
            if s.abs() < 1e-12 { json!([0.0, 0.0]) } else { json!([round4(x / s), round4(y / s)]) }
        };
        json!({
            "path": self.path,
            "description": self.profile.description,
            "kind": self.profile.kind.label(),
            "primaries": {"red": xy(0), "green": xy(1), "blue": xy(2)},
        })
    }
}

fn round4(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

/// Make `r` ready for `display`: an sRGB image (thumbnail, cached preview, camera JPEG) is
/// converted to the display's values. A render made for the display (see
/// [`RenderJob::with_display`](crate::media::RenderJob::with_display)) is already.
pub fn present(r: &mut RenderResult, display: &Display) {
    if r.display.is_some() {
        return;
    }
    if let Ok(rendered) = &mut r.rendered {
        match display.profile.from_srgb(&mut rendered.image) {
            Ok(()) => r.display = Some(display.id()),
            Err(e) => log::warn!("display profile: {e}"),
        }
    }
}

/// A render into `display`'s primaries (sRGB curve) → in place, the display's device values; and
/// its sRGB histogram, plus (`full_srgb`: for the cached view preview) its whole sRGB version.
/// The histogram samples the same pixels [`Histogram::of_srgb8`] would, so only those are
/// converted when the sRGB image itself isn't needed (slider drags).
pub(crate) fn finish_render(
    image: &mut Rgba8,
    display: &Display,
    full_srgb: bool,
    coverage: Option<&Coverage>,
) -> (Option<Rgba8>, Option<Histogram>) {
    let warn = |e: String| log::warn!("display profile: {e}");
    let (srgb, histogram) = if full_srgb {
        let srgb = display.profile.to_srgb(image).map_err(warn).ok();
        let h = srgb.as_ref().map(|s| Histogram::of_srgb8_covered(s, coverage));
        (srgb, h)
    } else {
        let sample = histogram_sample(image, coverage);
        (None, display.profile.to_srgb(&sample).map_err(warn).ok().map(|s| Histogram::of_srgb8(&s)))
    };
    if let Err(e) = display.profile.correct(image) {
        warn(e);
    }
    (srgb, histogram)
}

/// The pixels [`Histogram::of_srgb8_covered`] reads (at most ~1 MP, a regular grid, those `coverage`
/// says show the photo), as an image of their own (which it then reads whole).
fn histogram_sample(img: &Rgba8, coverage: Option<&Coverage>) -> Rgba8 {
    let coverage = coverage.filter(|c| c.width() == img.width && c.height() == img.height);
    let step = ((img.len() as f64 / 1_000_000.0).sqrt().ceil() as usize).max(1);
    if step == 1 && coverage.is_none() {
        return img.clone();
    }
    let (w, h) = (img.width.div_ceil(step), img.height.div_ceil(step));
    let mut data = Vec::with_capacity(w * h);
    for y in (0..img.height).step_by(step) {
        for x in (0..img.width).step_by(step) {
            if coverage.is_none_or(|c| c.covers(x, y)) {
                data.push(img.get(x, y));
            }
        }
    }
    // a pixel left out leaves the grid ragged: one row then (at most ~1 MP, so read whole)
    match coverage {
        Some(_) => Rgba8 { width: data.len(), height: 1, data },
        None => Rgba8 { width: w, height: h, data },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p3() -> Display {
        Display::from_bytes("p3.icc", &lcc_codecs::icc::write_named(lcc_codecs::NamedSpace::DisplayP3)).unwrap()
    }

    fn picture(w: usize, h: usize) -> Rgba8 {
        Rgba8 { width: w, height: h, data: (0..w * h).map(|i| [(i * 7 % 256) as u8, (i * 13 % 256) as u8, (i * 29 % 256) as u8, 255]).collect() }
    }

    /// The sampled histogram of a drag render is exactly the histogram of the whole sRGB image.
    #[test]
    fn sampled_histogram_matches_the_full_one() {
        let d = p3();
        for (w, h) in [(40, 30), (1500, 1100), (2561, 1709)] {
            let img = picture(w, h);
            let (mut a, mut b) = (img.clone(), img.clone());
            let (srgb, full) = finish_render(&mut a, &d, true, None);
            let (none, sampled) = finish_render(&mut b, &d, false, None);
            assert!(srgb.is_some() && none.is_none());
            assert_eq!(full.unwrap(), sampled.unwrap(), "{w}x{h}");
            assert_eq!(a, b);
        }
    }

    /// Issue #211 with a display profile: both histograms leave the blank border out, and agree.
    #[test]
    fn display_histograms_leave_the_blank_border_out() {
        let d = p3();
        for (w, h) in [(40, 30), (1500, 1100), (2561, 1709)] {
            let img = picture(w, h);
            let row = Coverage::row_words(w);
            let mut words = vec![0u32; row * h];
            for y in 0..h {
                for x in 0..w * 2 / 3 {
                    words[y * row + x / 32] |= 1 << (x % 32);
                }
            }
            let cov = Coverage::from_words(w, h, words).unwrap();
            let (mut a, mut b) = (img.clone(), img.clone());
            let full = finish_render(&mut a, &d, true, Some(&cov)).1.unwrap();
            let sampled = finish_render(&mut b, &d, false, Some(&cov)).1.unwrap();
            let all = finish_render(&mut img.clone(), &d, true, None).1.unwrap();
            assert!(full.total < all.total, "{w}x{h}: the border is left out");
            assert_eq!(full, sampled, "{w}x{h}");
        }
    }

    #[test]
    fn load_reports_and_rejects() {
        let dir = std::env::temp_dir().join(format!("app-display-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("p3.icc");
        std::fs::write(&good, lcc_codecs::icc::write_named(lcc_codecs::NamedSpace::DisplayP3)).unwrap();
        let bad = dir.join("bad.icc");
        std::fs::write(&bad, b"not a profile").unwrap();

        let d = Display::load_opt(&good.to_string_lossy()).unwrap().unwrap();
        let r = d.describe();
        assert_eq!(r["kind"], "matrix");
        assert_eq!(r["description"], "Display P3");
        let red = r["primaries"]["red"].as_array().unwrap();
        assert!((red[0].as_f64().unwrap() - 0.68).abs() < 2e-3 && (red[1].as_f64().unwrap() - 0.32).abs() < 2e-3, "{r}");
        assert!(Display::load_opt("  ").unwrap().is_none());
        let e = Display::load(&bad.to_string_lossy()).unwrap_err();
        assert!(e.contains("not a valid ICC profile"), "{e}");
        assert!(Display::load(&dir.join("missing.icc").to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
