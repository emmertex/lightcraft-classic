//! Which pixels of a rendered image show the photo.
//!
//! Lens corrections and perspective can pull the photo's edge inside the output frame; the pixels
//! past it are filled with a blank colour. They are part of the picture shown, but not of the photo,
//! so statistics such as the histogram leave them out (issue #211).

/// One bit per pixel of a `width × height` image (set: the pixel shows the photo), each row padded
/// to whole 32-bit words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    width: usize,
    height: usize,
    words: Vec<u32>,
}

impl Coverage {
    /// Words per row of a `width`-pixel-wide mask.
    pub fn row_words(width: usize) -> usize {
        width.div_ceil(32)
    }

    /// A mask from its words (`row_words(width) × height` of them, rows top to bottom, bit `x % 32` of word
    /// `x / 32` for pixel `x`). `None` when the count doesn't match.
    pub fn from_words(width: usize, height: usize, words: Vec<u32>) -> Option<Coverage> {
        (Self::row_words(width).checked_mul(height) == Some(words.len())).then_some(Coverage { width, height, words })
    }

    /// Every pixel of a `width × height` image shows the photo.
    pub fn full(width: usize, height: usize) -> Coverage {
        let row = Self::row_words(width);
        let mut words = vec![u32::MAX; row * height];
        if !width.is_multiple_of(32) {
            let last = (1u32 << (width % 32)) - 1;
            for w in words.iter_mut().skip(row.saturating_sub(1)).step_by(row.max(1)) {
                *w = last;
            }
        }
        Coverage { width, height, words }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The mask's words (see [`Self::from_words`]).
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Whether pixel `(x, y)` shows the photo (`false` outside the mask).
    pub fn covers(&self, x: usize, y: usize) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.words.get(y * Self::row_words(self.width) + x / 32).is_some_and(|w| w >> (x % 32) & 1 == 1)
    }

    /// Whether every pixel shows the photo.
    pub fn is_full(&self) -> bool {
        (0..self.height).all(|y| (0..self.width).step_by(32).all(|x| self.word_is_full(x, y)))
    }

    fn word_is_full(&self, x: usize, y: usize) -> bool {
        let n = (self.width - x).min(32);
        let full = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        self.words.get(y * Self::row_words(self.width) + x / 32).is_some_and(|w| w & full == full)
    }

    /// The `w × h` window at `(x0, y0)`, clamped to the mask the way [`Rgba8::crop`](crate::Rgba8::crop)
    /// clamps an image.
    pub fn crop(&self, x0: usize, y0: usize, w: usize, h: usize) -> Coverage {
        let x0 = x0.min(self.width);
        let y0 = y0.min(self.height);
        let w = w.min(self.width - x0);
        let h = h.min(self.height - y0);
        let row = Self::row_words(w);
        let mut words = vec![0u32; row * h];
        for y in 0..h {
            for x in 0..w {
                if self.covers(x0 + x, y0 + y)
                    && let Some(word) = words.get_mut(y * row + x / 32)
                {
                    *word |= 1 << (x % 32);
                }
            }
        }
        Coverage { width: w, height: h, words }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mask whose pixel `(x, y)` is set when `f(x, y)`.
    fn mask(w: usize, h: usize, f: impl Fn(usize, usize) -> bool) -> Coverage {
        let row = Coverage::row_words(w);
        let mut words = vec![0u32; row * h];
        for y in 0..h {
            for x in 0..w {
                if f(x, y) {
                    words[y * row + x / 32] |= 1 << (x % 32);
                }
            }
        }
        Coverage::from_words(w, h, words).unwrap()
    }

    #[test]
    fn reads_back_what_was_set() {
        let m = mask(70, 5, |x, y| (x + y) % 3 == 0);
        for y in 0..5 {
            for x in 0..70 {
                assert_eq!(m.covers(x, y), (x + y) % 3 == 0, "{x},{y}");
            }
        }
        assert!(!m.covers(70, 0) && !m.covers(0, 5), "outside the mask");
    }

    #[test]
    fn rejects_the_wrong_number_of_words() {
        assert!(Coverage::from_words(33, 2, vec![0; 3]).is_none());
        assert!(Coverage::from_words(33, 2, vec![0; 4]).is_some());
        assert!(Coverage::from_words(usize::MAX, 3, Vec::new()).is_none());
    }

    #[test]
    fn full_only_when_every_pixel_is_set() {
        assert!(mask(70, 3, |_, _| true).is_full());
        assert_eq!(Coverage::full(70, 3), mask(70, 3, |_, _| true));
        assert_eq!(Coverage::full(64, 2), mask(64, 2, |_, _| true));
        assert!(!mask(70, 3, |x, y| !(x == 69 && y == 2)).is_full());
        assert!(mask(0, 0, |_, _| false).is_full(), "an empty mask has nothing uncovered");
    }

    #[test]
    fn crop_keeps_the_window_and_clamps() {
        let m = mask(70, 6, |x, y| x >= 40 || y == 0);
        let c = m.crop(35, 0, 30, 4);
        assert_eq!((c.width(), c.height()), (30, 4));
        for y in 0..4 {
            for x in 0..30 {
                assert_eq!(c.covers(x, y), m.covers(35 + x, y), "{x},{y}");
            }
        }
        let c = m.crop(60, 4, 100, 100);
        assert_eq!((c.width(), c.height()), (10, 2));
        assert!(c.is_full());
    }
}
