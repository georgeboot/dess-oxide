//! Hourly weather features shared by the load and heat pump models.

use std::collections::BTreeMap;

use dess_core::Slot;
use dess_core::weather::Weather;

/// Time constants of the outdoor-temperature moving averages, hours. The heat
/// pump model learns how much of each the building "feels".
pub const EWMA_HOURS: [f64; 5] = [3.0, 6.0, 12.0, 24.0, 48.0];

/// Weather averaged over one hour, with the smoothed temperatures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HourWeather {
    /// Unix seconds of the hour's start.
    pub hour: i64,
    pub temperature: f64,
    pub humidity: f64,
    pub wind: f64,
    pub ghi: f64,
    /// Outdoor temperature smoothed over each of [`EWMA_HOURS`].
    pub smoothed_temperature: [f64; 5],
    /// Irradiance smoothed over six hours (solar gains through windows lag).
    pub smoothed_ghi: f64,
}

/// Hourly weather from slots: hours need all four slots. Moving averages
/// continue across gaps (they just don't update), which is harmless for the
/// odd missing hour.
pub fn hourly(weather: &BTreeMap<Slot, Weather>) -> Vec<HourWeather> {
    let mut by_hour: BTreeMap<i64, Vec<&Weather>> = BTreeMap::new();
    for (slot, w) in weather {
        by_hour
            .entry(slot.start_unix().div_euclid(3600) * 3600)
            .or_default()
            .push(w);
    }
    let mut out = Vec::with_capacity(by_hour.len());
    let mut smoothed: Option<([f64; 5], f64)> = None;
    for (hour, quarters) in by_hour {
        if quarters.len() != 4 {
            continue;
        }
        let mean = |f: fn(&Weather) -> f64| quarters.iter().map(|w| f(w)).sum::<f64>() / 4.0;
        let temperature = mean(|w| w.temperature);
        let ghi = mean(|w| w.ghi);
        let (temperatures, smoothed_ghi) = match smoothed {
            None => ([temperature; 5], ghi),
            Some((previous, previous_ghi)) => {
                let step =
                    |previous: f64, value: f64, tau: f64| previous + (value - previous) / tau;
                (
                    std::array::from_fn(|i| step(previous[i], temperature, EWMA_HOURS[i])),
                    step(previous_ghi, ghi, 6.0),
                )
            }
        };
        smoothed = Some((temperatures, smoothed_ghi));
        out.push(HourWeather {
            hour,
            temperature,
            humidity: mean(|w| w.humidity),
            wind: mean(|w| w.wind),
            ghi,
            smoothed_temperature: temperatures,
            smoothed_ghi,
        });
    }
    out
}

/// Every fifth day held out for validation. Days rather than a final block,
/// so every season is in both sets (heat pump use is very seasonal).
pub fn is_validation_hour(hour: i64) -> bool {
    hour.div_euclid(86_400) % 5 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_average_and_smooth() {
        let start = Slot::containing("2026-01-10T00:00:00Z".parse().unwrap());
        let mut weather = BTreeMap::new();
        let mut slot = start;
        for q in 0..12 {
            let temperature = if q < 4 { 0.0 } else { 12.0 };
            weather.insert(
                slot,
                Weather {
                    ghi: 0.0,
                    dni: 0.0,
                    dhi: 0.0,
                    temperature,
                    humidity: 90.0,
                    wind: 2.0,
                    others: [None; 2],
                },
            );
            slot = slot.next();
        }
        let hours = hourly(&weather);
        assert_eq!(hours.len(), 3);
        assert_eq!(hours[0].temperature, 0.0);
        // After a step from 0 to 12 °C the 3-hour average moves a third of the way.
        assert!((hours[1].smoothed_temperature[0] - 4.0).abs() < 1e-9);
        assert!(hours[1].smoothed_temperature[4] < hours[1].smoothed_temperature[0]);
    }
}
