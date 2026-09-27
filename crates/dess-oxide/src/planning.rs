//! Builds a plan from live data: the GX snapshot, stored prices and history,
//! and the baseline forecasts. Used by the `plan` command and, next, by the
//! shadow planner in `run`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use anyhow::Context;
use dess_core::battery::BatteryModel;
use dess_core::efficiency::LearnedLosses;
use dess_core::planner::{self, Plan, PlanRequest, PlannerSettings, SlotForecast};
use dess_core::solar::Orientation;
use dess_core::tariff::Tariff;
use dess_core::weather::{PvArray, Weather};
use dess_core::{EurPerKwh, Slot, WattHours, Watts, forecast, prices};
use dess_victron::Snapshot;
use dess_victron::reading::{self, BatteryInfo};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ToSpan};
use tracing::{debug, warn};

use crate::config::{CheapestStartConfig, Config, HistoryConfig, LocationConfig, RelayAction};
use crate::nordpool::NordPool;
use crate::store::Store;
use dess_models::features::HourWeather;
use dess_models::heatpump::HpModel;
use dess_models::load::LoadModel;

/// Days of price history used to estimate prices beyond the published ones.
pub const PRICE_LOOKBACK_DAYS: i32 = 14;
/// The horizon always reaches at least this far, with estimated prices.
const MIN_HORIZON_HOURS: i64 = 48;
/// Load used before any history has been recorded.
const FALLBACK_LOAD: Watts = Watts(600.0);

/// Nothing to plan with yet: normal right after a first start.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("no day-ahead prices yet")]
pub struct NoPrices;

/// The expected outage window from the page's settings, if one is set and
/// hasn't ended yet.
pub fn outage_window(store: &Store, now: Timestamp) -> Option<(Timestamp, Timestamp)> {
    if store.setting(OUTAGE_EXPECTED).ok().flatten().as_deref() != Some("on") {
        return None;
    }
    let start: Timestamp = store.setting(OUTAGE_START).ok().flatten()?.parse().ok()?;
    let hours: f64 = store.setting(OUTAGE_HOURS).ok().flatten()?.parse().ok()?;
    let end = start + SignedDuration::from_secs_f64(hours.clamp(0.25, 72.0) * 3600.0);
    (end > now).then_some((start, end))
}

/// When to start the flexible run (the dishwasher), from the plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheapestStart {
    pub start: Timestamp,
    pub end: Timestamp,
    /// The close of the night window it was chosen in.
    pub window_end: Timestamp,
    /// The run's expected cost at this start, €.
    pub cost: f64,
    /// The window's first possible start, and the run's cost there.
    pub first_start: Timestamp,
    pub first_cost: f64,
    /// Whether it relies on estimated prices (tomorrow's aren't out yet).
    pub estimated_price: bool,
}

/// The cheapest start for the configured run in the next night window that
/// it still fits in, at the marginal cost of its extra load under the plan's
/// policy: battery, PV and export included, not just the spot price.
pub fn cheapest_start(
    view: &PlanView,
    run: &CheapestStartConfig,
    tz: &TimeZone,
    now: Timestamp,
) -> Option<CheapestStart> {
    let duration = SignedDuration::from_secs_f64(run.hours * 3600.0);
    let today = now.to_zoned(tz.clone()).date();
    let at = |day: jiff::civil::Date, time| day.to_datetime(time).to_zoned(tz.clone()).ok();
    let (open, close) = (-1..=1).find_map(|offset: i64| {
        let day = today.checked_add(offset.days()).ok()?;
        let close_day = if run.finish_by <= run.earliest {
            day.tomorrow().ok()?
        } else {
            day
        };
        let open = at(day, run.earliest)?.timestamp().max(now);
        let close = at(close_day, run.finish_by)?.timestamp();
        (open + duration <= close).then_some((open, close))
    })?;

    let slots: Vec<Slot> = view.plan.slots.iter().map(|s| s.slot).collect();
    let costs = dess_core::control::marginal_costs(
        &view.plan,
        &view.forecasts,
        &view.battery,
        &view.settings,
        view.min_soc,
        Watts(run.kwh / run.hours * 1000.0),
    );
    let needed = (run.hours * 4.0).ceil() as usize;
    let (i, mean) = dess_core::control::cheapest_run(&slots, &costs, open, close, needed)?;
    let first = slots.iter().position(|s| s.start() >= open)?;
    let first_mean = costs.get(first..first + needed)?.iter().sum::<f64>() / needed as f64;
    let start = slots[i].start();
    Some(CheapestStart {
        start,
        end: start + duration,
        window_end: close,
        cost: mean * run.kwh,
        first_start: slots[first].start(),
        first_cost: first_mean * run.kwh,
        estimated_price: view.plan.slots[i..i + needed]
            .iter()
            .any(|s| s.estimated_price),
    })
}

pub const OUTAGE_EXPECTED: &str = "outage_expected";
pub const OUTAGE_START: &str = "outage_start";
pub const OUTAGE_HOURS: &str = "outage_hours";

/// What a price update found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriceUpdate {
    /// Whether any new prices were stored.
    pub changed: bool,
    /// Whether tomorrow's final prices are complete.
    pub tomorrow_complete: bool,
}

/// Fetches missing day-ahead prices: the lookback window, today and tomorrow.
pub async fn update_prices(
    store: &Mutex<Store>,
    nordpool: &NordPool,
    now: Timestamp,
    tz: &TimeZone,
) -> anyhow::Result<PriceUpdate> {
    let today = now.to_zoned(tz.clone()).date();
    let mut update = PriceUpdate::default();
    for offset in -PRICE_LOOKBACK_DAYS..=1 {
        let date = today.checked_add(offset.days())?;
        let from = Slot::containing(date.to_zoned(tz.clone())?.timestamp());
        let until = Slot::containing(date.tomorrow()?.to_zoned(tz.clone())?.timestamp());
        let expected = usize::try_from((until.start_unix() - from.start_unix()) / 900)?;
        if lock(store).final_price_count(from, until)? >= expected {
            update.tomorrow_complete |= offset == 1;
            continue;
        }
        match nordpool.day(date).await {
            Ok(Some(day)) => {
                lock(store).save_prices(&day.slots, day.is_final, "nordpool", now.as_second())?;
                update.changed = true;
                update.tomorrow_complete |=
                    offset == 1 && day.is_final && day.slots.len() >= expected;
            }
            Ok(None) => debug!(%date, "prices not published yet"),
            Err(error) => warn!(%date, "{error:#}"),
        }
    }
    Ok(update)
}

pub fn lock(store: &Mutex<Store>) -> std::sync::MutexGuard<'_, Store> {
    store.lock().expect("store lock poisoned")
}

/// A plan and everything it was made from.
#[derive(Debug, Clone)]
pub struct PlanView {
    pub planned_at: Timestamp,
    pub soc: f64,
    pub battery: BatteryModel,
    pub forecasts: Vec<SlotForecast>,
    pub settings: PlannerSettings,
    pub min_soc: f64,
    pub plan: Plan,
}

/// Learned models in use (each only once it beat its baseline).
#[derive(Debug, Default, Clone, Copy)]
pub struct Models<'a> {
    pub heat_pump: Option<&'a HpModel>,
    pub load: Option<&'a LoadModel>,
}

/// What the plan's forecasts are made from.
#[derive(Debug, Clone, Copy)]
pub struct ForecastInputs<'a> {
    pub pv: &'a BTreeMap<Slot, Watts>,
    /// The latest weather forecast.
    pub weather: &'a BTreeMap<Slot, Weather>,
    pub models: Models<'a>,
    /// An expected outage, `[start, end)`.
    pub outage: Option<(Timestamp, Timestamp)>,
}

/// During an expected outage, plan for more load and less PV than forecast,
/// so the battery holds out even when the forecast is off.
const OUTAGE_LOAD_MARGIN: f64 = 1.3;
const OUTAGE_PV_MARGIN: f64 = 0.7;

/// Plans from the current snapshot, stored prices and history.
pub fn make_plan(
    now: Timestamp,
    snapshot: &Snapshot,
    store: &Store,
    config: &Config,
    tariff: &Tariff,
    inputs: ForecastInputs<'_>,
) -> anyhow::Result<PlanView> {
    let soc = reading::sample(snapshot, now)?.soc_pct;
    let info = reading::battery_info(snapshot);
    let battery = battery_model(&info, config, &learned_losses(store, now)?)?;
    let lookback =
        Slot::containing(now - SignedDuration::from_hours(24 * i64::from(PRICE_LOOKBACK_DAYS + 1)));
    let prices = store.prices(
        lookback,
        Slot::containing(now + SignedDuration::from_hours(72)),
    )?;
    let history = load_history(store, &config.history, config.ev.on_input, lookback)?;
    let loads = |slots: &[Slot]| {
        house_load(
            slots,
            store,
            config,
            &tariff.time_zone,
            now,
            inputs,
            &history,
        )
    };
    let min_soc = min_soc(&info, config);
    let forecasts = slot_forecasts(
        now,
        &prices,
        tariff,
        loads,
        inputs.pv,
        min_soc,
        inputs.outage,
    )?;
    let mut forecasts = forecasts;
    if config.ev.on_input {
        // The EV isn't forecast, but a car charging now likely charges on
        // for the rest of this slot.
        let ev = reading::sample(snapshot, now)?.load_in.0;
        if let Some(first) = forecasts.first_mut().filter(|_| ev > 500.0) {
            first.load = Watts(first.load.0 + ev);
        }
    }
    let settings = planner_settings(config, &forecasts);
    let plan = planner::plan(&PlanRequest {
        now,
        soc_pct: soc,
        pv_on: pv_on(snapshot, config),
        battery: &battery,
        slots: &forecasts,
        settings: &settings,
    });
    Ok(PlanView {
        planned_at: now,
        soc,
        battery,
        forecasts,
        settings,
        min_soc,
        plan,
    })
}

/// Mean house load per slot since `from`, without the EV: recorded slots,
/// plus hours from Home Assistant's statistics where nothing was recorded
/// (each hour's mean spread over its four slots).
pub fn load_history(
    store: &Store,
    history: &HistoryConfig,
    ev_on_input: bool,
    from: Slot,
) -> anyhow::Result<Vec<(Slot, Watts)>> {
    let mut by_slot: BTreeMap<Slot, Watts> = store
        .load_history(from, !ev_on_input)?
        .into_iter()
        .collect();
    let entity = |role: &str| {
        history
            .entities()
            .into_iter()
            .find(|(r, _)| *r == role)
            .map(|(_, e)| e.to_owned())
    };
    let (Some(import), Some(export)) = (entity("grid_import"), entity("grid_export")) else {
        return Ok(by_slot.into_iter().collect());
    };
    let optional = ["pv", "battery_in", "battery_out", "ev"].map(entity);
    let mut entities = vec![import.as_str(), export.as_str()];
    entities.extend(optional.iter().flatten().map(String::as_str));
    for (hour, values) in store.ha_hourly(&entities, from.start())? {
        let get = |e: &Option<String>| e.as_ref().and_then(|e| values.get(e)).copied();
        let (Some(imported), Some(exported)) = (values.get(&import), values.get(&export)) else {
            continue;
        };
        // Every configured sensor must have a value, or the load would be wrong.
        if optional
            .iter()
            .zip(optional.iter().map(get))
            .any(|(e, v)| e.is_some() && v.is_none())
        {
            continue;
        }
        let [pv, battery_in, battery_out, ev] = optional.each_ref().map(|e| get(e).unwrap_or(0.0));
        let load_kwh = (imported - exported + pv - battery_in + battery_out - ev).max(0.0);
        let Some(first) = Slot::from_start_unix(hour) else {
            continue;
        };
        let mut slot = first;
        for _ in 0..4 {
            by_slot.entry(slot).or_insert(Watts(load_kwh * 1000.0));
            slot = slot.next();
        }
    }
    Ok(by_slot.into_iter().collect())
}

/// Losses learned from the last half year of steady-state samples.
pub fn learned_losses(store: &Store, now: Timestamp) -> anyhow::Result<LearnedLosses> {
    let today = now.as_second().div_euclid(86_400);
    Ok(dess_core::efficiency::fit_losses(
        &store.efficiency_bins(today - 180, today)?,
    ))
}

/// The battery model and current state, from the GX device, with learned
/// losses where there's enough data.
pub fn battery_model(
    info: &BatteryInfo,
    config: &Config,
    learned: &LearnedLosses,
) -> anyhow::Result<BatteryModel> {
    let capacity_wh = config
        .battery
        .capacity_kwh
        .map(|kwh| kwh * 1000.0)
        .or(info.capacity_wh)
        .context("battery capacity unknown: set battery.capacity_kwh")?;
    let units = info.inverter_units;
    let voltage = info.voltage.unwrap_or(51.2);
    // MultiPlus-II 48/5000: 70 A charger and 4 kW continuous per unit.
    let charge_current = info
        .max_charge_current
        .unwrap_or(f64::INFINITY)
        .min(70.0 * f64::from(units));
    let discharge_limit = info
        .max_discharge_current
        .map_or(f64::INFINITY, |a| a * voltage);
    let mut model = BatteryModel::multiplus_ii_prior(
        WattHours(capacity_wh),
        units,
        Watts(charge_current * voltage / 0.95),
        Watts(discharge_limit.min(4000.0 * f64::from(units))),
    );
    if let Some(standby) = learned.standby {
        model.standby = Watts(standby);
    }
    model.charge_loss = learned.charge.unwrap_or(model.charge_loss);
    model.discharge_loss = learned.discharge.unwrap_or(model.discharge_loss);
    Ok(model)
}

/// Whether PV is on right now, from the configured relay.
pub fn pv_on(snapshot: &Snapshot, config: &Config) -> bool {
    let Some(relay) = config.victron.pv_relay else {
        return true;
    };
    let closed = snapshot.number(&format!("system/0/Relay/{}/State", relay - 1)) == Some(1.0);
    match config.victron.pv_relay_energized {
        RelayAction::PvOff => !closed,
        RelayAction::PvOn => closed,
    }
}

/// Per-slot forecasts and prices from now to the end of the horizon.
pub fn slot_forecasts(
    now: Timestamp,
    prices: &BTreeMap<Slot, EurPerKwh>,
    tariff: &Tariff,
    loads: impl FnOnce(&[Slot]) -> anyhow::Result<Vec<Watts>>,
    pv: &BTreeMap<Slot, Watts>,
    min_soc: f64,
    outage: Option<(Timestamp, Timestamp)>,
) -> anyhow::Result<Vec<SlotForecast>> {
    let first = Slot::containing(now);
    let min_end = Slot::containing(now + SignedDuration::from_hours(MIN_HORIZON_HOURS));
    let until = prices
        .keys()
        .next_back()
        .map_or(min_end, |last| last.next().max(min_end));
    let spot = prices::horizon(prices, first, until, PRICE_LOOKBACK_DAYS.unsigned_abs())
        .ok_or(NoPrices)?;
    let slots: Vec<Slot> = spot.iter().map(|p| p.slot).collect();
    let loads = loads(&slots)?;
    spot.iter()
        .zip(loads)
        .map(|(price, load)| {
            let pv = pv.get(&price.slot).copied().unwrap_or(Watts::ZERO);
            let islanded = outage
                .is_some_and(|(start, end)| price.slot.end() > start && price.slot.start() < end);
            Ok(SlotForecast {
                slot: price.slot,
                load: if islanded {
                    Watts(load.0 * OUTAGE_LOAD_MARGIN)
                } else {
                    load
                },
                pv: if islanded {
                    Watts(pv.0 * OUTAGE_PV_MARGIN)
                } else {
                    pv
                },
                prices: tariff.prices(price.slot, price.spot)?,
                min_soc_end: min_soc,
                estimated_price: price.estimated,
                islanded,
            })
        })
        .collect()
}

/// House load per slot: the learned base-load and heat pump models where
/// they're in use and there's weather for the hour, else the history baseline.
fn house_load(
    slots: &[Slot],
    store: &Store,
    config: &Config,
    tz: &TimeZone,
    now: Timestamp,
    inputs: ForecastInputs<'_>,
    history: &[(Slot, Watts)],
) -> anyhow::Result<Vec<Watts>> {
    let baseline = forecast::baseline_load(history, slots, tz, FALLBACK_LOAD);
    let heat_pump_metered = config
        .history
        .heat_pump
        .as_deref()
        .is_some_and(|e| !e.is_empty());
    let Some(load) = inputs.models.load else {
        return Ok(baseline);
    };
    if heat_pump_metered && inputs.models.heat_pump.is_none() {
        return Ok(baseline);
    }
    // Stored recent weather first, so the moving averages carry on into the forecast.
    let mut weather = store.weather(
        Slot::containing(now - SignedDuration::from_hours(24 * 10)),
        Slot::containing(now),
    )?;
    weather.extend(inputs.weather.iter().map(|(slot, w)| (*slot, *w)));
    let hours: std::collections::HashMap<i64, HourWeather> =
        dess_models::features::hourly(&weather)
            .into_iter()
            .map(|h| (h.hour, h))
            .collect();
    Ok(slots
        .iter()
        .zip(baseline)
        .map(|(slot, fallback)| {
            let Some(w) = hours.get(&(slot.start_unix().div_euclid(3600) * 3600)) else {
                return fallback;
            };
            let Some((hour, weekday, holiday)) = crate::training::house_features(w, tz) else {
                return fallback;
            };
            let heat_pump = inputs.models.heat_pump.map_or(0.0, |m| m.hour_kwh(w, hour));
            Watts((load.hour_kwh(w, hour, weekday, holiday) + heat_pump) * 1000.0)
        })
        .collect())
}

/// Planner settings from the config. Energy left at the end of the horizon is
/// valued at 80 % of the median buy price, so the plan neither dumps the
/// battery nor hoards.
pub fn planner_settings(config: &Config, forecasts: &[SlotForecast]) -> PlannerSettings {
    let mut buy: Vec<f64> = forecasts.iter().map(|f| f.prices.buy.0).collect();
    buy.sort_by(f64::total_cmp);
    let median_buy = buy.get(buy.len() / 2).copied().unwrap_or(0.0);
    PlannerSettings {
        max_import: Watts(config.grid.max_import_kw * 1000.0),
        max_export: Watts(config.grid.max_export_kw * 1000.0),
        wear_cost: EurPerKwh(config.battery.wear_cost_eur_per_kwh),
        pv_switchable: config.victron.pv_relay.is_some(),
        terminal_value: EurPerKwh(0.8 * median_buy.max(0.0)),
        ..PlannerSettings::default()
    }
}

/// A plain-text table of the first `rows` slots.
pub fn render(plan: &Plan, forecasts: &[SlotForecast], tz: &TimeZone, rows: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<11} {:>6} {:>6} {:>6} {:>6} {:>7} {:>7} {:>5}  PV",
        "slot", "buy", "sell", "load", "pv", "battery", "grid", "SoC"
    );
    let _ = writeln!(
        out,
        "{:<11} {:>6} {:>6} {:>6} {:>6} {:>7} {:>7} {:>5}",
        "", "€/kWh", "€/kWh", "kW", "kW", "kW", "kW", "%"
    );
    for (slot, forecast) in plan.slots.iter().zip(forecasts).take(rows) {
        let time = slot.slot.start().to_zoned(tz.clone());
        let _ = writeln!(
            out,
            "{} {:>6.3}{} {:>6.3} {:>6.2} {:>6.2} {:>+7.2} {:>+7.2} {:>5.0}  {}",
            time.strftime("%a %H:%M"),
            slot.prices.buy.0,
            if slot.estimated_price { "~" } else { " " },
            slot.prices.sell.0,
            forecast.load.0 / 1000.0,
            forecast.pv.0 / 1000.0,
            slot.battery_ac.0 / 1000.0,
            slot.grid.0 / 1000.0,
            slot.soc_end,
            if slot.pv_on { "on" } else { "OFF" },
        );
    }
    let horizon_end = plan.slots.last().map(|s| s.slot.end().to_zoned(tz.clone()));
    let _ = writeln!(
        out,
        "\n{} slots until {}; expected cost €{:.2} (~ = estimated price)",
        plan.slots.len(),
        horizon_end.map_or_else(String::new, |t| t.strftime("%a %H:%M").to_string()),
        plan.expected_cost
    );
    out
}

/// The SoC floor for planning: ESS's minimum and the configured reserve.
pub fn min_soc(info: &BatteryInfo, config: &Config) -> f64 {
    info.active_min_soc
        .unwrap_or(10.0)
        .max(config.battery.reserve_soc)
}

/// Where the system is: from the config, or Home Assistant's location.
pub async fn resolve_location(
    client: &reqwest::Client,
    config: &Config,
) -> anyhow::Result<LocationConfig> {
    match (
        config.location,
        crate::homeassistant::Endpoint::resolve(config),
    ) {
        (Some(location), _) => Ok(location),
        (None, Some(endpoint)) => crate::homeassistant::location(client, &endpoint).await,
        (None, None) => anyhow::bail!("no location: set [location] or [homeassistant]"),
    }
}

/// Expected PV power per slot from the weather: the learned model when
/// there is one, else the configured arrays.
pub fn pv_from_weather(
    weather: &BTreeMap<Slot, Weather>,
    config: &Config,
    location: LocationConfig,
    learned: Option<&dess_models::pv::PvModel>,
) -> BTreeMap<Slot, Watts> {
    let arrays: Vec<PvArray> = config
        .pv
        .iter()
        .map(|a| PvArray {
            kwp: a.kwp,
            orientation: Orientation {
                tilt: a.tilt,
                azimuth: a.azimuth,
            },
        })
        .collect();
    let (latitude, longitude) = (location.latitude, location.longitude);
    weather
        .iter()
        .map(|(slot, w)| {
            let power = match learned {
                Some(model) => Watts(
                    model.power_kw(&dess_models::pv::Quarter::new(
                        *slot, w, latitude, longitude,
                    )) * 1000.0,
                ),
                None => dess_core::weather::baseline_pv(*slot, w, &arrays, latitude, longitude),
            };
            (*slot, power)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hourly_history_fills_gaps_but_recorded_slots_win() {
        let mut store = Store::in_memory().unwrap();
        let hour: Timestamp = "2026-09-26T10:00:00Z".parse().unwrap();
        let rows: Vec<(String, Timestamp, f64)> = [
            ("sensor.import", 1.0),
            ("sensor.export", 0.2),
            ("sensor.pv", 1.5),
            ("sensor.bat_in", 0.8),
            ("sensor.bat_out", 0.1),
        ]
        .into_iter()
        .map(|(e, kwh)| (e.to_owned(), hour, kwh))
        .collect();
        store.save_ha_hourly(&rows).unwrap();
        let history = HistoryConfig {
            grid_import: Some("sensor.import".into()),
            grid_export: Some("sensor.export".into()),
            pv: Some("sensor.pv".into()),
            battery_in: Some("sensor.bat_in".into()),
            battery_out: Some("sensor.bat_out".into()),
            heat_pump: None,
            ev: None,
        };
        let from = Slot::containing("2026-09-26T00:00:00Z".parse().unwrap());
        let loads = load_history(&store, &history, false, from).unwrap();
        assert_eq!(loads.len(), 4);
        // 1.0 − 0.2 + 1.5 − 0.8 + 0.1 = 1.6 kWh in an hour.
        assert!(loads.iter().all(|(_, w)| (w.0 - 1600.0).abs() < 1e-9));

        // A sensor without a value for the hour makes the hour unusable.
        store
            .save_ha_hourly(&[(
                "sensor.import".into(),
                hour + SignedDuration::from_hours(1),
                1.0,
            )])
            .unwrap();
        assert_eq!(
            load_history(&store, &history, false, from).unwrap().len(),
            4
        );
    }
}
