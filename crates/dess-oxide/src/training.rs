//! Training the learned models from stored history.

use std::collections::BTreeMap;

use dess_core::Slot;
use dess_core::battery::BatteryModel;
use dess_core::calendar;
use dess_core::weather::Weather;
use dess_models::features::{self, HourWeather};
use dess_models::heatpump::{self, HpFit, HpHour, HpModel};
use dess_models::hot_water::{self, HotWaterDay, HotWaterModel};
use dess_models::load::{self, Dense, LoadFit, LoadHour, LoadModel};
use dess_models::pv::{self, Array, FitReport, Hour, PvModel, Quarter};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::json;

use crate::config::{Config, LocationConfig};
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
    let recorded = store.recorded_pv(from, config.victron.pv_relay_state())?;
    let ha: BTreeMap<i64, f64> = match config.history.pv.as_deref().filter(|e| !e.is_empty()) {
        Some(entity) => store
            .ha_hourly(&[entity], from.start())?
            .into_iter()
            .filter_map(|(hour, values)| values.get(entity).map(|kwh| (hour, *kwh)))
            .filter(|(_, kwh)| !crate::planning::implausible(*kwh))
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

/// The promoted models in the store.
#[derive(Debug, Default)]
pub struct StoredModels {
    pub pv: Option<PvModel>,
    pub heat_pump: Option<HpModel>,
    pub load: Option<LoadModel>,
    pub hot_water: Option<HotWaterModel>,
}

impl StoredModels {
    pub fn load(store: &Store) -> Self {
        fn promoted<T>(
            store: &Store,
            name: &str,
            parse: fn(&serde_json::Value) -> Option<T>,
        ) -> Option<T> {
            match store.model(name) {
                Ok(Some(stored)) if stored.promoted => parse(&stored.params),
                Ok(_) => None,
                Err(error) => {
                    tracing::warn!("loading the {name} model: {error:#}");
                    None
                }
            }
        }
        Self {
            pv: promoted(store, "pv", pv_model_from_json),
            heat_pump: promoted(store, "heat_pump", hp_model_from_json),
            load: promoted(store, "load", load_model_from_json),
            hot_water: promoted(store, "hot_water", hot_water_model_from_json),
        }
    }

    pub fn as_models(&self) -> crate::planning::Models<'_> {
        crate::planning::Models {
            heat_pump: self.heat_pump.as_ref(),
            load: self.load.as_ref(),
            hot_water: self.hot_water.as_ref(),
        }
    }
}

/// Hourly training data for the house: base load (everything but the heat
/// pump) and the heat pump, each with the hour's weather.
#[derive(Debug, Default)]
pub struct HouseHours {
    pub load: Vec<LoadHour>,
    pub heat_pump: Vec<HpHour>,
    /// Days with OpenAmber's mode known all day.
    pub hot_water: Vec<HotWaterDay>,
}

type HotWaterDays = BTreeMap<jiff::civil::Date, ([f64; 24], f64, usize)>;

/// Hot water per hour (hours with the mode known throughout) and, per day,
/// hot water by local hour without legionella, legionella, and labelled
/// slots, from the split heat pump energy.
fn hot_water_labels(
    store: &Store,
    tz: &TimeZone,
    from: Slot,
) -> anyhow::Result<(BTreeMap<i64, f64>, HotWaterDays)> {
    let mut hours: BTreeMap<i64, (f64, u8)> = BTreeMap::new();
    let mut days: HotWaterDays = BTreeMap::new();
    for s in store.heat_pump_modes(from)? {
        if !s.fully_labelled() {
            continue;
        }
        let hour = hours
            .entry(s.slot.start_unix().div_euclid(3600) * 3600)
            .or_default();
        hour.0 += s.hot_water_wh / 1000.0;
        hour.1 += 1;
        let local = s.slot.start().to_zoned(tz.clone());
        let day = days.entry(local.date()).or_insert(([0.0; 24], 0.0, 0));
        day.0[usize::from(local.hour().unsigned_abs())] +=
            (s.hot_water_wh - s.legionella_wh) / 1000.0;
        day.1 += s.legionella_wh / 1000.0;
        day.2 += 1;
    }
    let hours = hours
        .into_iter()
        .filter(|(_, (_, slots))| *slots == 4)
        .map(|(hour, (kwh, _))| (hour, kwh))
        .collect();
    Ok((hours, days))
}

pub fn house_hours(
    store: &Store,
    config: &Config,
    battery: Option<&BatteryModel>,
    tz: &TimeZone,
    now: Timestamp,
) -> anyhow::Result<HouseHours> {
    let from = Slot::containing(
        crate::openmeteo::HISTORY_START
            .to_zoned(TimeZone::UTC)?
            .timestamp(),
    );
    let weather = features::hourly(&store.weather(from, Slot::containing(now))?);

    // Total house load per hour, where all four slots are known.
    let mut totals: BTreeMap<i64, (f64, u8)> = BTreeMap::new();
    for (slot, watts) in
        crate::planning::load_history(store, &config.history, config.ev.on_input, battery, from)?
    {
        let entry = totals
            .entry(slot.start_unix().div_euclid(3600) * 3600)
            .or_default();
        entry.0 += watts.0 * 0.25 / 1000.0;
        entry.1 += 1;
    }
    let heat_pump_entity = config
        .history
        .heat_pump
        .as_deref()
        .filter(|e| !e.is_empty());
    let heat_pump: BTreeMap<i64, f64> = match heat_pump_entity {
        Some(entity) => store
            .ha_hourly(&[entity], from.start())?
            .into_iter()
            .filter_map(|(hour, values)| values.get(entity).map(|kwh| (hour, *kwh)))
            .filter(|(_, kwh)| !crate::planning::implausible(*kwh))
            .collect(),
        None => BTreeMap::new(),
    };

    let (hot_water_hours, hot_water_days) = match config.openamber() {
        Some(_) => hot_water_labels(store, tz, from)?,
        None => Default::default(),
    };
    let mut day_temperatures: BTreeMap<jiff::civil::Date, (f64, u32)> = BTreeMap::new();

    let mut out = HouseHours::default();
    for w in weather {
        let local = Timestamp::from_second(w.hour)?.to_zoned(tz.clone());
        let local_hour = local.hour().unsigned_abs();
        let day = day_temperatures.entry(local.date()).or_default();
        day.0 += w.temperature;
        day.1 += 1;
        if let Some(&kwh) = heat_pump.get(&w.hour) {
            out.heat_pump.push(HpHour {
                weather: w,
                local_hour,
                energy_kwh: kwh,
                hot_water_kwh: hot_water_hours.get(&w.hour).copied(),
            });
        }
        let Some(&(total, 4)) = totals.get(&w.hour) else {
            continue;
        };
        let base = match heat_pump_entity {
            Some(_) => match heat_pump.get(&w.hour) {
                Some(hp) => total - hp,
                None => continue,
            },
            None => total,
        };
        if base >= 0.0 {
            out.load.push(LoadHour {
                weather: w,
                local_hour,
                weekday: local.weekday().to_monday_zero_offset().unsigned_abs(),
                holiday: calendar::is_holiday(local.date()),
                energy_kwh: base,
            });
        }
    }
    // Days labelled nearly throughout (a few slots of gap are fine).
    out.hot_water = hot_water_days
        .into_iter()
        .filter(|(_, (_, _, slots))| *slots >= 88)
        .filter_map(|(date, (by_hour, legionella, _))| {
            let (sum, n) = day_temperatures.get(&date)?;
            (*n >= 20).then(|| HotWaterDay {
                date,
                mean_temperature: sum / f64::from(*n),
                by_hour,
                legionella_kwh: legionella,
            })
        })
        .collect();
    Ok(out)
}

/// Fits the heat pump and base-load models, each when there's enough history.
pub fn train_house(
    store: &Store,
    config: &Config,
    battery: Option<&BatteryModel>,
    tz: &TimeZone,
    now: Timestamp,
) -> anyhow::Result<HouseFits> {
    let hours = house_hours(store, config, battery, tz, now)?;
    let heat_pump =
        (hours.heat_pump.len() >= MIN_HOURS).then(|| heatpump::fit(&hours.heat_pump, 1500));
    let load = (hours.load.len() >= MIN_HOURS).then(|| load::fit(&hours.load, 1500));
    let hot_water = hot_water::fit(&hours.hot_water);
    Ok(HouseFits {
        heat_pump,
        load,
        hot_water,
    })
}

pub struct HouseFits {
    pub heat_pump: Option<HpFit>,
    pub load: Option<LoadFit>,
    /// With OpenAmber and a week of its days.
    pub hot_water: Option<HotWaterModel>,
}

pub fn hot_water_model_json(m: &HotWaterModel) -> serde_json::Value {
    json!({
        "base_kwh": m.base_kwh,
        "per_degree_kwh": m.per_degree_kwh,
        "profile": m.profile,
        "legionella_kwh": m.legionella_kwh,
        "days": m.days,
        "daily_mae": m.daily_mae,
    })
}

pub fn hot_water_model_from_json(v: &serde_json::Value) -> Option<HotWaterModel> {
    let profile: Vec<f64> = v["profile"]
        .as_array()?
        .iter()
        .map(serde_json::Value::as_f64)
        .collect::<Option<_>>()?;
    Some(HotWaterModel {
        base_kwh: v["base_kwh"].as_f64()?,
        per_degree_kwh: v["per_degree_kwh"].as_f64()?,
        profile: profile.try_into().ok()?,
        legionella_kwh: v["legionella_kwh"].as_f64()?,
        days: usize::try_from(v["days"].as_u64()?).ok()?,
        daily_mae: v["daily_mae"].as_f64()?,
    })
}

/// The weather features the house models need for a given hour.
pub fn house_features(hour: &HourWeather, tz: &TimeZone) -> Option<(u8, u8, bool)> {
    let local = Timestamp::from_second(hour.hour).ok()?.to_zoned(tz.clone());
    Some((
        local.hour().unsigned_abs(),
        local.weekday().to_monday_zero_offset().unsigned_abs(),
        calendar::is_holiday(local.date()),
    ))
}

pub fn hp_model_json(m: &HpModel) -> serde_json::Value {
    json!({
        "lag_weights": m.lag_weights,
        "ua_kw_per_k": m.ua_kw_per_k,
        "balance_c": m.balance_c,
        "wind_factor": m.wind_factor,
        "solar_gain": m.solar_gain,
        "cop_c0": m.cop_c0,
        "cop_c1": m.cop_c1,
        "frost_factor": m.frost_factor,
        "coil_delta_k": m.coil_delta_k,
        "hot_water_kwh": m.hot_water_kwh,
        "standby_kwh": m.standby_kwh,
    })
}

pub fn hp_model_from_json(v: &serde_json::Value) -> Option<HpModel> {
    let array = |key: &str| -> Option<Vec<f64>> {
        v[key]
            .as_array()?
            .iter()
            .map(serde_json::Value::as_f64)
            .collect()
    };
    let number = |key: &str| v[key].as_f64();
    Some(HpModel {
        lag_weights: array("lag_weights")?.try_into().ok()?,
        ua_kw_per_k: number("ua_kw_per_k")?,
        balance_c: number("balance_c")?,
        wind_factor: number("wind_factor")?,
        solar_gain: number("solar_gain")?,
        cop_c0: number("cop_c0")?,
        cop_c1: number("cop_c1")?,
        frost_factor: number("frost_factor")?,
        coil_delta_k: number("coil_delta_k")?,
        hot_water_kwh: array("hot_water_kwh")?.try_into().ok()?,
        // Models from before 0.13 had standby in the hour-of-day term.
        standby_kwh: number("standby_kwh").unwrap_or(0.0),
    })
}

pub fn load_model_json(m: &LoadModel) -> serde_json::Value {
    json!({
        "layers": m.layers.iter().map(|d| json!({
            "inputs": d.inputs, "outputs": d.outputs, "weights": d.weights, "bias": d.bias,
        })).collect::<Vec<_>>(),
    })
}

pub fn load_model_from_json(v: &serde_json::Value) -> Option<LoadModel> {
    let layers: Vec<Dense> = v["layers"]
        .as_array()?
        .iter()
        .map(|d| {
            let numbers = |key: &str| -> Option<Vec<f64>> {
                d[key]
                    .as_array()?
                    .iter()
                    .map(serde_json::Value::as_f64)
                    .collect()
            };
            Some(Dense {
                inputs: usize::try_from(d["inputs"].as_u64()?).ok()?,
                outputs: usize::try_from(d["outputs"].as_u64()?).ok()?,
                weights: numbers("weights")?,
                bias: numbers("bias")?,
            })
        })
        .collect::<Option<_>>()?;
    Some(LoadModel {
        layers: layers.try_into().ok()?,
    })
}

/// Metrics for a model with a naive baseline to beat.
pub fn baseline_metrics(
    hours: usize,
    validation_mae: f64,
    baseline_mae: f64,
    mean_kwh: f64,
) -> serde_json::Value {
    json!({
        "hours": hours,
        "validation_mae_kwh": validation_mae,
        "baseline_mae_kwh": baseline_mae,
        "mean_kwh": mean_kwh,
    })
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

    #[test]
    fn house_models_round_trip_through_json() {
        let hp = HpModel::initial();
        assert_eq!(hp_model_from_json(&hp_model_json(&hp)), Some(hp));
        let layer = |i: usize, o: usize| Dense {
            inputs: i,
            outputs: o,
            weights: vec![0.5; i * o],
            bias: vec![0.1; o],
        };
        let load = LoadModel {
            layers: [layer(16, 32), layer(32, 32), layer(32, 1)],
        };
        assert_eq!(load_model_from_json(&load_model_json(&load)), Some(load));
    }
}
