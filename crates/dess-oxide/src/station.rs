//! A local weather station (such as an Ecowitt WS90) read from Home
//! Assistant: its readings every 30 seconds, stored as means per slot. The
//! planner compares the last hour with the forecast and corrects the next
//! hours (see `dess_core::weather_correction`).

use std::sync::Arc;
use std::time::Duration;

use dess_core::Slot;
use dess_core::weather_correction::Observation;
use jiff::Timestamp;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::homeassistant::{self, Endpoint};
use crate::planning::lock;
use crate::run::Shared;

const POLL: Duration = Duration::from_secs(30);

/// The station's latest readings, converted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub at: Timestamp,
    pub now: Observation,
}

/// Running sums for the current slot.
#[derive(Debug, Default)]
struct Means {
    sums: [f64; 4],
    counts: [u32; 4],
}

impl Means {
    fn add(&mut self, o: &Observation) {
        for (i, value) in [o.temperature, o.humidity, o.wind, o.ghi]
            .into_iter()
            .enumerate()
        {
            if let Some(v) = value {
                self.sums[i] += v;
                self.counts[i] += 1;
            }
        }
    }

    fn observation(&self) -> Option<Observation> {
        let mean =
            |i: usize| (self.counts[i] > 0).then(|| self.sums[i] / f64::from(self.counts[i]));
        let o = Observation {
            temperature: mean(0),
            humidity: mean(1),
            wind: mean(2),
            ghi: mean(3),
        };
        (o != Observation::default()).then_some(o)
    }
}

pub async fn run(shared: Arc<Shared>, client: reqwest::Client, mut stop: watch::Receiver<bool>) {
    let sensors = shared.config.weather_station.entities();
    if sensors.is_empty() {
        return;
    }
    let Some(endpoint) = Endpoint::resolve(&shared.config) else {
        warn!("weather_station is set, but there's no Home Assistant to read it from");
        return;
    };
    info!(sensors = sensors.len(), "reading the local weather station");
    let mut slot: Option<(Slot, Means)> = None;
    let mut failing = false;
    let mut tick = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.wait_for(|stopping| *stopping) => return,
        }
        let now = Timestamp::now();
        let mut reading = Observation::default();
        for (what, entity) in &sensors {
            match homeassistant::state_with_unit(&client, &endpoint, entity).await {
                Ok(Some((state, unit))) => {
                    let value = state
                        .parse::<f64>()
                        .ok()
                        .and_then(|v| convert(what, v, unit.as_deref()));
                    match *what {
                        "temperature" => reading.temperature = value,
                        "humidity" => reading.humidity = value,
                        "wind_speed" => reading.wind = value,
                        _ => reading.ghi = value,
                    }
                    failing = false;
                }
                Ok(None) => {}
                Err(error) => {
                    if !failing {
                        warn!("reading the weather station: {error:#}");
                        failing = true;
                    }
                }
            }
        }
        *shared.station.lock().expect("lock poisoned") = Some(Reading {
            at: now,
            now: reading,
        });

        let current = Slot::containing(now);
        if let Some((done, means)) = slot.take_if(|(s, _)| *s != current)
            && let Some(o) = means.observation()
            && let Err(error) = lock(&shared.store).save_observation(done, &o)
        {
            warn!(%error, "storing the weather station's readings");
        }
        slot.get_or_insert_with(|| (current, Means::default()))
            .1
            .add(&reading);
    }
}

/// A reading in dess-oxide's units (°C, %, m/s, W/m²), from HA's unit.
fn convert(what: &str, value: f64, unit: Option<&str>) -> Option<f64> {
    let unit = unit.unwrap_or("").trim();
    let converted = match (what, unit) {
        ("temperature", "°F") => (value - 32.0) * 5.0 / 9.0,
        ("temperature", "K") => value - 273.15,
        ("wind_speed", "km/h") => value / 3.6,
        ("wind_speed", "mph") => value * 0.447_04,
        ("wind_speed", "kn") => value * 0.514_444,
        ("wind_speed", "ft/s") => value * 0.3048,
        // Daylight is roughly 120 lux per W/m².
        ("solar_radiation", "lx") => value / 120.0,
        ("solar_radiation", "klx") => value * 1000.0 / 120.0,
        // Already °C, %, m/s or W/m².
        ("temperature" | "humidity" | "wind_speed" | "solar_radiation", _) => value,
        _ => return None,
    };
    converted.is_finite().then_some(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_units() {
        assert!((convert("temperature", 50.0, Some("°F")).unwrap() - 10.0).abs() < 1e-9);
        assert!((convert("wind_speed", 36.0, Some("km/h")).unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(convert("wind_speed", 4.0, Some("m/s")), Some(4.0));
        assert!((convert("solar_radiation", 12_000.0, Some("lx")).unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(convert("solar_radiation", 340.0, Some("W/m²")), Some(340.0));
        assert_eq!(convert("temperature", f64::NAN, None), None);
    }

    #[test]
    fn means_per_slot() {
        let mut m = Means::default();
        m.add(&Observation {
            temperature: Some(2.0),
            ghi: Some(100.0),
            ..Observation::default()
        });
        m.add(&Observation {
            temperature: Some(4.0),
            ..Observation::default()
        });
        let o = m.observation().unwrap();
        assert_eq!(o.temperature, Some(3.0));
        assert_eq!(o.ghi, Some(100.0));
        assert_eq!(o.humidity, None);
        assert!(Means::default().observation().is_none());
    }
}
