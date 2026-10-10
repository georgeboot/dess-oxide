//! Correcting the weather forecast with a local weather station.
//!
//! A forecast model is right on average but can be off at your house on the
//! day: fog it didn't see, a colder night, clouds an hour early. A station on
//! site shows that. For each plan we compare the last hour it measured with
//! what the forecast said for that hour, and shift the next hours by the
//! difference, fading out with lead time:
//!
//! - temperature and humidity by their difference, fading over hours;
//! - irradiance by the ratio of measured to forecast, fading within about an
//!   hour (clouds move on). The ratio is taken relative to the station's usual
//!   ratio, so a sensor that reads high or low doesn't skew PV forecasts.
//!
//! Past slots get the measured temperature and humidity, so moving averages
//! (the house's thermal lag) start from what actually happened. Wind isn't
//! corrected: a station a few metres up doesn't measure the 10 m wind the
//! models use.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};

use crate::slot::Slot;
use crate::weather::Weather;

/// A slot's mean measurements; `None` where the station has no such sensor.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Observation {
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
    pub wind: Option<f64>,
    pub ghi: Option<f64>,
}

/// How far off the forecast was over the last hour.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Correction {
    /// Measured minus forecast, °C.
    pub temperature: Option<f64>,
    /// Measured minus forecast, percentage points.
    pub humidity: Option<f64>,
    /// Measured over forecast irradiance, relative to the usual ratio.
    pub irradiance: Option<f64>,
}

/// Corrections fade as `e^(−lead / τ)`.
const TEMPERATURE_TAU_HOURS: f64 = 3.0;
const IRRADIANCE_TAU_HOURS: f64 = 1.0;
/// How far back the recent comparison and the usual ratio look.
const RECENT: SignedDuration = SignedDuration::from_hours(1);
const USUAL: SignedDuration = SignedDuration::from_hours(24 * 7);
/// Irradiance ratios only mean something with some sun.
const MIN_GHI: f64 = 50.0;
const USUAL_MIN_GHI: f64 = 200.0;

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
    (n >= 2).then(|| sum / f64::from(n))
}

/// The correction for plans made at `now`.
pub fn correction(
    forecast: &BTreeMap<Slot, Weather>,
    observed: &BTreeMap<Slot, Observation>,
    now: Timestamp,
) -> Correction {
    let pairs = |since: SignedDuration| {
        observed
            .range(Slot::containing(now - since)..Slot::containing(now))
            .filter_map(|(slot, o)| Some((forecast.get(slot)?, o)))
            .collect::<Vec<_>>()
    };
    let recent = pairs(RECENT);
    let temperature = mean(
        recent
            .iter()
            .filter_map(|(f, o)| Some(o.temperature? - f.temperature)),
    );
    let humidity = mean(
        recent
            .iter()
            .filter_map(|(f, o)| Some(o.humidity? - f.humidity)),
    );

    let ratio = |pairs: &[(&Weather, &Observation)], min_ghi: f64| {
        let (measured, forecast): (Vec<f64>, Vec<f64>) = pairs
            .iter()
            .filter(|(f, _)| f.ghi > min_ghi)
            .filter_map(|(f, o)| Some((o.ghi?, f.ghi)))
            .unzip();
        (measured.len() >= 2).then(|| measured.iter().sum::<f64>() / forecast.iter().sum::<f64>())
    };
    let usual = ratio(&pairs(USUAL), USUAL_MIN_GHI).filter(|r| *r > 0.05);
    let irradiance = ratio(&recent, MIN_GHI).map(|r| (r / usual.unwrap_or(1.0)).clamp(0.2, 2.0));
    Correction {
        temperature,
        humidity,
        irradiance,
    }
}

/// The forecast with `correction` applied to the slots from `now` on, and
/// the measured temperature and humidity in the slots before.
pub fn apply(
    forecast: &BTreeMap<Slot, Weather>,
    observed: &BTreeMap<Slot, Observation>,
    correction: Correction,
    now: Timestamp,
) -> BTreeMap<Slot, Weather> {
    let current = Slot::containing(now);
    forecast
        .iter()
        .map(|(slot, w)| {
            let mut w = *w;
            if *slot < current {
                if let Some(o) = observed.get(slot) {
                    w.temperature = o.temperature.unwrap_or(w.temperature);
                    w.humidity = o.humidity.unwrap_or(w.humidity);
                }
            } else {
                let lead = slot.start().duration_since(now).as_secs_f64().max(0.0) / 3600.0;
                let fade = |tau: f64| (-lead / tau).exp();
                if let Some(bias) = correction.temperature {
                    w.temperature += bias * fade(TEMPERATURE_TAU_HOURS);
                }
                if let Some(bias) = correction.humidity {
                    w.humidity =
                        (w.humidity + bias * fade(TEMPERATURE_TAU_HOURS)).clamp(0.0, 100.0);
                }
                if let Some(ratio) = correction.irradiance {
                    let factor = 1.0 + (ratio - 1.0) * fade(IRRADIANCE_TAU_HOURS);
                    w.ghi *= factor;
                    w.dni *= factor;
                    w.dhi *= factor;
                }
            }
            (*slot, w)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather(temperature: f64, ghi: f64) -> Weather {
        Weather {
            ghi,
            dni: ghi,
            dhi: ghi / 4.0,
            temperature,
            humidity: 80.0,
            wind: 3.0,
            others: [None; 2],
        }
    }

    fn slots(from: Timestamp, n: usize) -> Vec<Slot> {
        std::iter::successors(Some(Slot::containing(from)), |s| Some(s.next()))
            .take(n)
            .collect()
    }

    #[test]
    fn a_colder_morning_carries_on_for_a_while() {
        let now: Timestamp = "2026-10-01T08:00:00Z".parse().unwrap();
        let all = slots(now - SignedDuration::from_hours(2), 4 * 16);
        let forecast: BTreeMap<Slot, Weather> =
            all.iter().map(|&s| (s, weather(5.0, 0.0))).collect();
        // The last two hours were 2 °C colder and more humid than forecast.
        let observed: BTreeMap<Slot, Observation> = all[..8]
            .iter()
            .map(|&s| {
                (
                    s,
                    Observation {
                        temperature: Some(3.0),
                        humidity: Some(95.0),
                        ..Observation::default()
                    },
                )
            })
            .collect();
        let c = correction(&forecast, &observed, now);
        assert!((c.temperature.unwrap() + 2.0).abs() < 1e-9);
        assert!((c.humidity.unwrap() - 15.0).abs() < 1e-9);
        assert_eq!(c.irradiance, None, "no sun, no ratio");
        let corrected = apply(&forecast, &observed, c, now);
        let at =
            |h: i64| corrected[&Slot::containing(now + SignedDuration::from_hours(h))].temperature;
        assert!((at(0) - 3.0).abs() < 1e-9, "now: fully corrected");
        assert!(at(3) > 3.0 && at(3) < 5.0, "fading: {}", at(3));
        assert!((at(12) - 5.0).abs() < 0.05, "all but gone after half a day");
        // The past carries the measurements.
        assert_eq!(corrected[&all[0]].temperature, 3.0);
    }

    #[test]
    fn irradiance_is_judged_against_the_stations_usual_ratio() {
        let now: Timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        let all = slots(now - SignedDuration::from_hours(48), 4 * 54);
        let forecast: BTreeMap<Slot, Weather> =
            all.iter().map(|&s| (s, weather(15.0, 500.0))).collect();
        // The sensor always reads 10 % high; the last hour was half as sunny.
        let observed: BTreeMap<Slot, Observation> = all
            .iter()
            .filter(|s| s.start() < now)
            .map(|&s| {
                let recent = s.start() >= now - SignedDuration::from_hours(1);
                let ghi = if recent { 275.0 } else { 550.0 };
                (
                    s,
                    Observation {
                        ghi: Some(ghi),
                        ..Observation::default()
                    },
                )
            })
            .collect();
        let c = correction(&forecast, &observed, now);
        let ratio = c.irradiance.unwrap();
        assert!((ratio - 0.5).abs() < 0.03, "{ratio}");
        let corrected = apply(&forecast, &observed, c, now);
        let now_ghi = corrected[&Slot::containing(now)].ghi;
        let later = corrected[&Slot::containing(now + SignedDuration::from_hours(4))].ghi;
        assert!(now_ghi < 270.0 && later > 490.0, "{now_ghi} {later}");
    }
}
