//! Power and energy newtypes.
//!
//! Internally everything is W and Wh; conversions to kW or kWh only happen at
//! the edges (config, display).

use std::ops::{Add, AddAssign, Neg, Sub};

/// Instantaneous power in watts. Sign conventions are documented where a
/// value is produced (e.g. grid power is positive for import).
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
#[must_use]
pub struct Watts(pub f64);

/// Energy in watt-hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd)]
#[must_use]
pub struct WattHours(pub f64);

impl Watts {
    pub const ZERO: Self = Self(0.0);

    /// Energy delivered by this power held for `seconds`.
    pub fn over_seconds(self, seconds: f64) -> WattHours {
        WattHours(self.0 * seconds / 3600.0)
    }

    /// The positive part (e.g. import out of a signed grid power).
    pub fn positive_part(self) -> Self {
        Self(self.0.max(0.0))
    }

    /// The magnitude of the negative part (e.g. export out of a signed grid power).
    pub fn negative_part(self) -> Self {
        Self((-self.0).max(0.0))
    }

    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }
}

impl WattHours {
    pub const ZERO: Self = Self(0.0);
}

macro_rules! impl_arith {
    ($t:ty) => {
        impl Add for $t {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }
        impl AddAssign for $t {
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }
        impl Sub for $t {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }
        impl Neg for $t {
            type Output = Self;
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }
        impl std::iter::Sum for $t {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                Self(iter.map(|v| v.0).sum())
            }
        }
    };
}

impl_arith!(Watts);
impl_arith!(WattHours);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_over_time_is_energy() {
        assert_eq!(Watts(3600.0).over_seconds(1.0), WattHours(1.0));
        assert_eq!(Watts(1000.0).over_seconds(900.0), WattHours(250.0));
    }

    #[test]
    fn parts_split_signed_power() {
        assert_eq!(Watts(-300.0).positive_part(), Watts(0.0));
        assert_eq!(Watts(-300.0).negative_part(), Watts(300.0));
        assert_eq!(Watts(200.0).negative_part(), Watts(0.0));
    }
}
