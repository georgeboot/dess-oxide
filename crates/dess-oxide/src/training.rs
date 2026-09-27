//! Training the learned models from stored history.

use std::collections::BTreeMap;

use dess_core::Slot;
use dess_core::weather::Weather;
use dess_models::pv::{self, Array, FitReport, Hour, PvModel, Quarter};
use jiff::Timestamp;
use serde_json::json;

use crate::config::{Config, LocationConfig, RelayAction};
use crate::store::Store;

/// Configured arrays lose about this much before AC; the learned kWp
/// includes it.
const PERFORMANCE_RATIO: f64 = 0.88;
/// Fewer usable hours than this and there's nothing worth fitting.
const MIN_HOURS: usize = 24 * 14;
const ITERATIONS: usize = 800;

/// The configured arrays as a starting model, or `None` without arrays.
pub fn initial_pv_model(config: &Config) -> Option<PvModel> {
    if config.pv.is_empty() {
        return None;
    }
    let total: f64 = config.pv.iter().map(|a| a.kwp).sum();
    Some(PvModel {
        arrays: config
            .pv
            .iter()
            .map(|a| Array {
                kwp: a.kwp * PERFORMANCE_RATIO,
                tilt: a.tilt,
                azimuth: a.azimuth,
            })
            .collect(),
        // Unknown: start well above the arrays' output, so it's learned only if it binds.
        cap_kw: total * 1.2,
    })
}

/// Hours with complete weather and a known PV output: our own recorded
/// quarter hours where all four exist, Home Assistant's statistics otherwise.
pub fn pv_hours(
    store: &Store,
    config: &Config,
    location: LocationConfig,
    now: Timestamp,
) -> anyhow::Result<Vec<Hour>> {
    let from = Slot::containing(
        crate::openmeteo::HISTORY_START
            .to_zoned(jiff::tz::TimeZone::UTC)?
            .timestamp(),
    );
    let until = Slot::containing(now);
    let weather = store.weather(from, until)?;
    let relay = config.victron.pv_relay.map(|relay| {
        let closed_means_on = config.victron.pv_relay_energized == RelayAction::PvOn;
        (usize::from(relay - 1), closed_means_on)
    });
    let recorded = store.recorded_pv(from, relay)?;
    let ha: BTreeMap<i64, f64> = match config.history.pv.as_deref().filter(|e| !e.is_empty()) {
        Some(entity) => store
            .ha_hourly(&[entity], from.start())?
            .into_iter()
            .filter_map(|(hour, values)| values.get(entity).map(|kwh| (hour, *kwh)))
            .collect(),
        None => BTreeMap::new(),
    };

    let mut hours = Vec::new();
    let first_hour = from.start_unix().div_euclid(3600) * 3600;
    for hour_start in (first_hour..until.start_unix()).step_by(3600) {
        let slots: Vec<Slot> = (0..4)
            .filter_map(|q| Slot::from_start_unix(hour_start + q * 900))
            .collect();
        let Some(quarters) = quarters(&slots, &weather, location) else {
            continue;
        };
        let recorded_kwh: Option<f64> = slots
            .iter()
            .map(|s| recorded.get(s).copied())
            .sum::<Option<f64>>()
            .map(|wh| wh / 1000.0);
        let Some(energy_kwh) = recorded_kwh.or_else(|| ha.get(&hour_start).copied()) else {
            continue;
        };
        hours.push(Hour {
            quarters,
            energy_kwh,
        });
    }
    Ok(hours)
}

fn quarters(
    slots: &[Slot],
    weather: &BTreeMap<Slot, Weather>,
    location: LocationConfig,
) -> Option<[Quarter; 4]> {
    let mut out = [Quarter::default(); 4];
    for (q, slot) in out.iter_mut().zip(slots) {
        *q = Quarter::new(
            *slot,
            weather.get(slot)?,
            location.latitude,
            location.longitude,
        );
    }
    (slots.len() == 4).then_some(out)
}

/// Fits the PV model, or `None` when there isn't enough history yet.
pub fn train_pv(
    store: &Store,
    config: &Config,
    location: LocationConfig,
    now: Timestamp,
) -> anyhow::Result<Option<FitReport>> {
    let Some(initial) = initial_pv_model(config) else {
        return Ok(None);
    };
    let hours: Vec<Hour> = pv_hours(store, config, location, now)?
        .into_iter()
        // Hours where the output collapsed although the sun was out are
        // curtailment or outages, not physics.
        .filter(|h| {
            let expected = initial.hour_kwh(h);
            expected < 0.5 || h.energy_kwh > 0.02 * expected
        })
        .collect();
    if hours.len() < MIN_HOURS {
        return Ok(None);
    }
    Ok(Some(pv::fit(&hours, &initial, ITERATIONS)))
}

pub fn pv_model_json(model: &PvModel) -> serde_json::Value {
    json!({
        "arrays": model.arrays.iter().map(|a| json!({ "kwp": a.kwp, "tilt": a.tilt, "azimuth": a.azimuth })).collect::<Vec<_>>(),
        "cap_kw": model.cap_kw,
    })
}

pub fn pv_model_from_json(value: &serde_json::Value) -> Option<PvModel> {
    let arrays = value["arrays"]
        .as_array()?
        .iter()
        .map(|a| {
            Some(Array {
                kwp: a["kwp"].as_f64()?,
                tilt: a["tilt"].as_f64()?,
                azimuth: a["azimuth"].as_f64()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(PvModel {
        arrays,
        cap_kw: value["cap_kw"].as_f64()?,
    })
}

pub fn fit_metrics(report: &FitReport) -> serde_json::Value {
    json!({
        "hours": report.hours,
        "validation_mae_kwh": report.validation_mae,
        "configured_validation_mae_kwh": report.initial_validation_mae,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
            [victron]
            host = "x"
            [[pv]]
            kwp = 5.0
            tilt = 30
            azimuth = 180
            [history]
            pv = "sensor.pv"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn hours_combine_weather_with_ha_statistics() {
        let mut store = Store::in_memory().unwrap();
        let hour: Timestamp = "2026-06-01T10:00:00Z".parse().unwrap();
        let weather: BTreeMap<Slot, Weather> = (0..8)
            .map(|q| {
                let slot = Slot::containing(hour + jiff::SignedDuration::from_mins(15 * q));
                (
                    slot,
                    Weather {
                        ghi: 600.0,
                        dni: 500.0,
                        dhi: 150.0,
                        temperature: 18.0,
                        humidity: 60.0,
                        wind: 2.0,
                    },
                )
            })
            .collect();
        store.save_weather(&weather, true, hour).unwrap();
        // Only the first hour has a PV value in HA.
        store
            .save_ha_hourly(&[("sensor.pv".into(), hour, 3.2)])
            .unwrap();
        let location = LocationConfig {
            latitude: 52.29,
            longitude: 5.79,
        };
        let now: Timestamp = "2026-06-02T00:00:00Z".parse().unwrap();
        let hours = pv_hours(&store, &config(), location, now).unwrap();
        assert_eq!(hours.len(), 1);
        assert_eq!(hours[0].energy_kwh, 3.2);
        assert!(hours[0].quarters.iter().all(|q| q.ghi == 600.0));
    }

    #[test]
    fn the_initial_model_folds_in_system_losses() {
        let model = initial_pv_model(&config()).unwrap();
        assert!((model.arrays[0].kwp - 4.4).abs() < 1e-9);
        assert!(model.cap_kw > 5.0);
    }

    #[test]
    fn model_json_round_trips() {
        let model = PvModel {
            arrays: vec![Array {
                kwp: 4.9,
                tilt: 33.0,
                azimuth: 193.0,
            }],
            cap_kw: 8.1,
        };
        assert_eq!(pv_model_from_json(&pv_model_json(&model)), Some(model));
    }
}
