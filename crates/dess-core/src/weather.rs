//! Weather per slot, and the baseline PV forecast computed from it.

use crate::slot::Slot;
use crate::solar::{self, Irradiance, Orientation};
use crate::units::Watts;

/// Weather for one 15-minute slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weather {
    /// Global horizontal irradiance, W/m² (mean over the slot).
    pub ghi: f64,
    /// Direct normal irradiance, W/m².
    pub dni: f64,
    /// Diffuse horizontal irradiance, W/m².
    pub dhi: f64,
    /// Air temperature, °C.
    pub temperature: f64,
    /// Relative humidity, %.
    pub humidity: f64,
    /// Wind speed at 10 m, m/s.
    pub wind: f64,
}

/// A PV array as configured: peak power and orientation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PvArray {
    pub kwp: f64,
    pub orientation: Orientation,
}

/// System losses other than temperature: inverter, wiring, soiling.
const PERFORMANCE_RATIO: f64 = 0.88;
/// Power temperature coefficient of crystalline silicon, per °C.
const TEMPERATURE_COEFFICIENT: f64 = -0.004;

/// Cell temperature (Faiman model with typical coefficients), °C.
pub fn cell_temperature(poa: f64, air: f64, wind: f64) -> f64 {
    air + poa / (25.0 + 6.84 * wind.max(0.0))
}

/// Expected PV power for a slot from the configured arrays: the baseline
/// until the learned model takes over.
pub fn baseline_pv(
    slot: Slot,
    weather: &Weather,
    arrays: &[PvArray],
    latitude: f64,
    longitude: f64,
) -> Watts {
    let middle =
        slot.start() + jiff::SignedDuration::from_mins(7) + jiff::SignedDuration::from_secs(30);
    let sun = solar::sun_position(middle, latitude, longitude);
    if !sun.is_up() {
        return Watts::ZERO;
    }
    let irradiance = Irradiance {
        ghi: weather.ghi,
        dni: weather.dni,
        dhi: weather.dhi,
    };
    let extraterrestrial = solar::extraterrestrial(middle);
    arrays
        .iter()
        .map(|array| {
            let poa = solar::plane_of_array(sun, irradiance, array.orientation, extraterrestrial);
            let cell = cell_temperature(poa, weather.temperature, weather.wind);
            let temperature_factor = 1.0 + TEMPERATURE_COEFFICIENT * (cell - 25.0);
            Watts(array.kwp * poa * PERFORMANCE_RATIO * temperature_factor)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sunny_summer_noon_gives_most_of_peak() {
        let slot = Slot::containing("2026-06-21T11:30:00Z".parse().unwrap());
        let weather = Weather {
            ghi: 850.0,
            dni: 800.0,
            dhi: 150.0,
            temperature: 22.0,
            humidity: 50.0,
            wind: 3.0,
        };
        let south = PvArray {
            kwp: 5.0,
            orientation: Orientation {
                tilt: 30.0,
                azimuth: 180.0,
            },
        };
        let power = baseline_pv(slot, &weather, &[south], 52.3, 5.8);
        assert!((3500.0..5000.0).contains(&power.0), "{}", power.0);
        let night = Slot::containing("2026-06-21T23:00:00Z".parse().unwrap());
        assert_eq!(
            baseline_pv(night, &weather, &[south], 52.3, 5.8),
            Watts::ZERO
        );
    }
}
