//! Capture-absolute sample offsets, distinct from slice indices.
//!
//! A [`Frames`] value counts mono samples from the first frame of the current
//! capture. Indices into a *snapshot* of that capture stay plain `usize`,
//! because a snapshot may begin after the capture did.

use std::{
    fmt,
    ops::{Add, AddAssign},
};

/// Mono samples since the start of the current capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frames(pub usize);

impl Frames {
    pub const ZERO: Self = Self(0);

    pub fn get(self) -> usize {
        self.0
    }

    pub fn seconds(self, rate: u32) -> f64 {
        self.0 as f64 / f64::from(rate)
    }

    /// Distance from an earlier offset, clamped at zero. A lagging hint may
    /// legitimately sit behind the authoritative offset.
    pub fn since(self, earlier: Self) -> usize {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<usize> for Frames {
    type Output = Self;
    fn add(self, samples: usize) -> Self {
        Self(self.0 + samples)
    }
}

impl AddAssign<usize> for Frames {
    fn add_assign(&mut self, samples: usize) {
        self.0 += samples;
    }
}

impl fmt::Display for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_saturating_and_seconds_use_the_rate() {
        assert_eq!(Frames(8).since(Frames(3)), 5);
        assert_eq!(
            Frames(3).since(Frames(8)),
            0,
            "a lagging hint is not negative"
        );
        assert_eq!((Frames(16_000) + 8_000).seconds(16_000), 1.5);
        assert!(Frames::ZERO < Frames(1));
    }
}
