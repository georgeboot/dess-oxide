//! Builds a plan from live data: the GX snapshot, stored prices and history,
//! and the baseline forecasts. Used by the `plan` command and, next, by the
//! shadow planner in `run`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use anyhow::Context;
use dess_core::battery::BatteryModel;
use dess_core::capacity::CapacityFit;
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
use crate::store::{SlotFlows, Store};
use dess_models::features::HourWeather;
use dess_models::heatpump::HpModel;
use dess_models::hot_water::HotWaterModel;
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
    /// The heat pump's part of each forecast's load, when modelled.
    pub heat_pump: Vec<Option<Watts>>,
    pub settings: PlannerSettings,
    pub min_soc: f64,
    pub plan: Plan,
}

/// Learned models in use (each only once it beat its baseline).
#[derive(Debug, Default, Clone, Copy)]
pub struct Models<'a> {
    pub heat_pump: Option<&'a HpModel>,
    pub load: Option<&'a LoadModel>,
    /// Hot water, forecast apart from heating (OpenAmber).
    pub hot_water: Option<&'a HotWaterModel>,
}

/// What the plan is made from, besides the GX snapshot and the store.
#[derive(Debug, Clone, Copy)]
pub struct ForecastInputs<'a> {
    pub pv: &'a BTreeMap<Slot, Watts>,
    /// The latest weather forecast.
    pub weather: &'a BTreeMap<Slot, Weather>,
    pub models: Models<'a>,
    /// An expected outage, `[start, end)`.
    pub outage: Option<(Timestamp, Timestamp)>,
    /// A finer SoC than the BMS reports, if the recorder has one (%).
    pub soc: Option<f64>,
    /// When OpenAmber's next legionella run is due.
    pub next_legionella: Option<Timestamp>,
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
    let soc = match inputs.soc {
        Some(soc) => soc,
        None => reading::sample(snapshot, now)?.soc_pct,
    };
    let info = reading::battery_info(snapshot);
    let learned = learned_capacity(store, now)?.usable_wh();
    let mut battery = battery_model(&info, config, &learned_losses(store, now)?, learned)?;
    if let Some(draw) = learned_bypass_draw(store) {
        battery.bypass_draw = Some(Watts(draw));
    }
    let lookback =
        Slot::containing(now - SignedDuration::from_hours(24 * i64::from(PRICE_LOOKBACK_DAYS + 1)));
    let prices = store.prices(
        lookback,
        Slot::containing(now + SignedDuration::from_hours(72)),
    )?;
    let history = load_history(
        store,
        &config.history,
        config.ev.on_input,
        Some(&battery),
        lookback,
    )?;
    let mut heat_pump = Vec::new();
    let loads = |slots: &[Slot]| {
        let loads = house_load(
            slots,
            store,
            config,
            &tariff.time_zone,
            now,
            inputs,
            &history,
        )?;
        heat_pump = loads.iter().map(|l| l.heat_pump).collect();
        Ok(loads.into_iter().map(|l| l.total).collect())
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
        heat_pump,
        settings,
        min_soc,
        plan,
    })
}

/// Mean house load per slot since `from`, without the EV: recorded slots,
/// plus hours from Home Assistant's statistics where nothing was recorded
/// (each hour's mean spread over its four slots).
///
/// The battery's part comes from its DC counters (the BMS's) through
/// `battery`'s loss curves and standby draw; without a model, DC counts as
/// AC. Without both counters, only the recordings count: the derived load
/// would be wrong whenever the battery moved.
pub fn load_history(
    store: &Store,
    history: &HistoryConfig,
    ev_on_input: bool,
    battery: Option<&BatteryModel>,
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
    if entity("battery_dc_in").is_none() || entity("battery_dc_out").is_none() {
        return Ok(by_slot.into_iter().collect());
    }
    let optional = ["pv", "battery_dc_in", "battery_dc_out", "ev"].map(entity);
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
        let (battery_in, battery_out) = match battery {
            Some(model) => ac_from_dc(model, battery_in, battery_out),
            None => (battery_in, battery_out),
        };
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

/// The AC energy (kWh) into and out of the inverters in an hour that moved
/// `dc_in` and `dc_out` kWh at the battery, each at a steady power over the
/// hour. The standby draw is AC in.
fn ac_from_dc(battery: &BatteryModel, dc_in: f64, dc_out: f64) -> (f64, f64) {
    let into = battery
        .charge_loss
        .ac_for_charging(dc_in * 1000.0)
        .map_or(dc_in, |w| w / 1000.0);
    let out = battery.discharge_loss.ac_from_discharging(dc_out * 1000.0) / 1000.0;
    (into + battery.standby.0 / 1000.0, out)
}

/// What recorded slots cost at the tariff's prices, and what they would have
/// cost without the battery: the same load and PV, netted per slot.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Money {
    pub actual: f64,
    pub without_battery: f64,
    /// Hours recorded (with prices).
    pub hours: f64,
}

pub fn money(flows: &[SlotFlows], spot: &BTreeMap<Slot, EurPerKwh>, tariff: &Tariff) -> Money {
    let kwh = |wh: WattHours| wh.0 / 1000.0;
    let mut money = Money::default();
    for f in flows {
        let Some(prices) = spot
            .get(&f.slot)
            .and_then(|&s| tariff.prices(f.slot, s).ok())
        else {
            continue;
        };
        let (buy, sell) = (prices.buy.0, prices.sell.0);
        money.actual += kwh(f.import) * buy - kwh(f.export) * sell;
        let net = kwh(f.load) - kwh(f.pv);
        money.without_battery += net.max(0.0) * buy - (-net).max(0.0) * sell;
        money.hours += f.covered_seconds / 3600.0;
    }
    money
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
/// The inverters' measured draw in bypass, as `sum,samples` (fading).
pub const BYPASS_DRAW: &str = "bypass_draw";
/// Samples (seconds) needed before the measured bypass draw is used.
const MIN_BYPASS_SAMPLES: f64 = 600.0;

/// The stored bypass draw statistics: `(sum of watts, samples)`.
pub fn bypass_draw_stats(store: &Store) -> (f64, f64) {
    store
        .setting(BYPASS_DRAW)
        .ok()
        .flatten()
        .and_then(|s| {
            let (sum, n) = s.split_once(',')?;
            Some((sum.parse().ok()?, n.parse().ok()?))
        })
        .unwrap_or((0.0, 0.0))
}

/// The inverters' draw in bypass, once measured long enough.
pub fn learned_bypass_draw(store: &Store) -> Option<f64> {
    let (sum, n) = bypass_draw_stats(store);
    (n >= MIN_BYPASS_SAMPLES).then(|| sum / n)
}

/// Usable capacity learned from the last half year's long SoC stretches.
pub fn learned_capacity(store: &Store, now: Timestamp) -> anyhow::Result<CapacityFit> {
    let from = Slot::containing(now - SignedDuration::from_hours(24 * 180));
    Ok(dess_core::capacity::fit_capacity(&store.slot_energy(from)?))
}

/// Usable capacity: configured, else learned, else from the GX device.
pub fn capacity_wh(info: &BatteryInfo, config: &Config, learned: Option<f64>) -> Option<f64> {
    config
        .battery
        .capacity_kwh
        .map(|kwh| kwh * 1000.0)
        .or(learned)
        .or(info.capacity_wh)
}

pub fn battery_model(
    info: &BatteryInfo,
    config: &Config,
    learned: &LearnedLosses,
    learned_capacity: Option<f64>,
) -> anyhow::Result<BatteryModel> {
    let capacity_wh = capacity_wh(info, config, learned_capacity)
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
/// The load forecast for one slot, and the heat pump's part of it when a
/// heat pump model made it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LoadForecast {
    total: Watts,
    heat_pump: Option<Watts>,
}

/// House load per slot. With a metered heat pump whose model is in use, the
/// load is base load (the base-load model, or else history without the heat
/// pump) plus the heat pump model. Otherwise it's the base-load model if in
/// use, else history.
fn house_load(
    slots: &[Slot],
    store: &Store,
    config: &Config,
    tz: &TimeZone,
    now: Timestamp,
    inputs: ForecastInputs<'_>,
    history: &[(Slot, Watts)],
) -> anyhow::Result<Vec<LoadForecast>> {
    let heat_pump_entity = config
        .history
        .heat_pump
        .as_deref()
        .filter(|e| !e.is_empty());
    let heat_pump_model = inputs
        .models
        .heat_pump
        .filter(|_| heat_pump_entity.is_some());
    // The base-load model is trained without the heat pump, so it needs the
    // heat pump model next to it when the heat pump is metered.
    let load_model = inputs
        .models
        .load
        .filter(|_| heat_pump_entity.is_none() || heat_pump_model.is_some());
    let plain = |loads: Vec<Watts>| {
        loads
            .into_iter()
            .map(|total| LoadForecast {
                total,
                heat_pump: None,
            })
            .collect()
    };
    if heat_pump_model.is_none() && load_model.is_none() {
        return Ok(plain(forecast::baseline_load(
            history,
            slots,
            tz,
            FALLBACK_LOAD,
        )));
    }

    // History without the heat pump, for base load where the model isn't in use.
    let baseline = match (heat_pump_entity, heat_pump_model) {
        (Some(entity), Some(_)) => {
            let from = history.first().map_or(Slot::containing(now), |(s, _)| *s);
            let hourly = store.ha_hourly(&[entity], from.start())?;
            let base: Vec<(Slot, Watts)> = history
                .iter()
                .filter_map(|&(slot, total)| {
                    let hour = slot.start_unix().div_euclid(3600) * 3600;
                    let kwh = hourly.get(&hour)?.get(entity)?;
                    Some((slot, Watts((total.0 - kwh * 1000.0).max(0.0))))
                })
                .collect();
            forecast::baseline_load(&base, slots, tz, FALLBACK_LOAD)
        }
        _ => forecast::baseline_load(history, slots, tz, FALLBACK_LOAD),
    };

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
    let hot_water = match inputs
        .models
        .hot_water
        .filter(|_| heat_pump_model.is_some())
    {
        Some(model) => Some(hot_water_forecast(
            model,
            slots,
            &hours,
            store,
            tz,
            now,
            inputs.next_legionella,
        )?),
        None => None,
    };
    Ok(slots
        .iter()
        .zip(baseline)
        .enumerate()
        .map(|(i, (slot, base_history))| {
            let features = hours
                .get(&(slot.start_unix().div_euclid(3600) * 3600))
                .and_then(|w| Some((w, crate::training::house_features(w, tz)?)));
            let Some((w, (hour, weekday, holiday))) = features else {
                return LoadForecast {
                    total: base_history,
                    heat_pump: None,
                };
            };
            let base = load_model.map_or(base_history.0, |m| {
                m.hour_kwh(w, hour, weekday, holiday) * 1000.0
            });
            let heat_pump = heat_pump_model.map(|m| match &hot_water {
                Some(hot_water) => Watts(m.heating_kwh(w) * 1000.0 + hot_water[i]),
                None => Watts(m.hour_kwh(w, hour) * 1000.0),
            });
            LoadForecast {
                total: Watts(base + heat_pump.map_or(0.0, |h| h.0)),
                heat_pump,
            }
        })
        .collect())
}

/// Hot water per slot, W: each local day's expected energy (from its mean
/// temperature) spread over the learned hours of the day. For today, only
/// what's left after what already ran. Plus the next legionella run, at
/// its announced time, at about 2 kW.
fn hot_water_forecast(
    model: &HotWaterModel,
    slots: &[Slot],
    hours: &std::collections::HashMap<i64, HourWeather>,
    store: &Store,
    tz: &TimeZone,
    now: Timestamp,
    next_legionella: Option<Timestamp>,
) -> anyhow::Result<Vec<f64>> {
    let local = |slot: &Slot| slot.start().to_zoned(tz.clone());
    let mut temperatures: BTreeMap<jiff::civil::Date, (f64, u32)> = BTreeMap::new();
    for w in hours.values() {
        let date = Timestamp::from_second(w.hour)?.to_zoned(tz.clone()).date();
        let entry = temperatures.entry(date).or_default();
        entry.0 += w.temperature;
        entry.1 += 1;
    }
    let daily = |date: jiff::civil::Date| {
        let mean = temperatures
            .get(&date)
            .map_or(10.0, |(sum, n)| sum / f64::from(*n));
        model.daily_kwh(mean)
    };
    let weight = |slot: &Slot| model.profile[usize::from(local(slot).hour().unsigned_abs())] / 4.0;

    let today = now.to_zoned(tz.clone()).date();
    let done_today: f64 = store
        .heat_pump_modes(Slot::containing(today.to_zoned(tz.clone())?.timestamp()))?
        .iter()
        .map(|s| (s.hot_water_wh - s.legionella_wh) / 1000.0)
        .sum();
    let left_today = (daily(today) - done_today).max(0.0);
    let today_weight: f64 = slots
        .iter()
        .filter(|s| local(s).date() == today)
        .map(weight)
        .sum();

    let mut watts: Vec<f64> = slots
        .iter()
        .map(|slot| {
            let date = local(slot).date();
            let kwh = if date == today {
                if today_weight > 1e-9 {
                    left_today * weight(slot) / today_weight
                } else {
                    0.0
                }
            } else {
                daily(date) * weight(slot)
            };
            kwh * 4000.0
        })
        .collect();

    if let Some(start) = next_legionella.filter(|_| model.legionella_kwh > 0.0) {
        const SLOT_KWH: f64 = 0.5; // about 2 kW
        let mut left = model.legionella_kwh;
        for (slot, w) in slots.iter().zip(&mut watts) {
            if slot.end() <= start || left <= 0.0 {
                continue;
            }
            let kwh = left.min(SLOT_KWH);
            *w += kwh * 4000.0;
            left -= kwh;
        }
    }
    Ok(watts)
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
    fn hot_water_is_what_is_left_today_plus_legionella() {
        use dess_core::heat_pump_modes::ModeSlot;
        let mut store = Store::in_memory().unwrap();
        let tz = TimeZone::UTC;
        let now: Timestamp = "2026-09-27T12:00:00Z".parse().unwrap();
        // 2 kWh a day, all of it 11:00–13:00 (half each hour).
        let mut profile = [0.0; 24];
        profile[11] = 0.5;
        profile[12] = 0.5;
        let model = HotWaterModel {
            base_kwh: 2.0,
            per_degree_kwh: 0.0,
            profile,
            legionella_kwh: 1.0,
            days: 10,
            daily_mae: 0.0,
        };
        // 1.5 kWh already ran this morning.
        let morning = Slot::containing("2026-09-27T11:00:00Z".parse().unwrap());
        store
            .save_heat_pump_modes(&[ModeSlot {
                slot: morning,
                covered_seconds: 900.0,
                labelled_seconds: 900.0,
                total_wh: 1500.0,
                hot_water_wh: 1500.0,
                legionella_wh: 0.0,
            }])
            .unwrap();
        let slots: Vec<Slot> =
            std::iter::successors(Some(Slot::containing(now)), |s| Some(s.next()))
                .take(4 * 36)
                .collect();
        let legionella: Timestamp = "2026-09-28T15:00:00Z".parse().unwrap();
        let watts = hot_water_forecast(
            &model,
            &slots,
            &std::collections::HashMap::new(),
            &store,
            &tz,
            now,
            Some(legionella),
        )
        .unwrap();
        let kwh = |from: usize, to: usize| watts[from..to].iter().sum::<f64>() / 4000.0;
        // Today: the 0.5 kWh left, in 12:00–13:00.
        assert!((kwh(0, 4) - 0.5).abs() < 1e-9, "{:?}", &watts[..8]);
        assert!(kwh(4, 48).abs() < 1e-9);
        // Tomorrow: 2 kWh at 11–13 plus the 1 kWh legionella run from 15:00.
        let tomorrow = 48; // slots from 00:00 on the 28th
        assert!((kwh(tomorrow + 44, tomorrow + 52) - 2.0).abs() < 1e-9);
        assert!((kwh(tomorrow + 60, tomorrow + 62) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dc_counters_become_ac_with_the_losses() {
        let battery = BatteryModel::multiplus_ii_prior(
            WattHours(30_000.0),
            3,
            Watts(10_000.0),
            Watts(12_000.0),
        );
        // 3 kWh into the cells took more than 3 kWh of AC; 2 kWh out gave less.
        let (into, out) = ac_from_dc(&battery, 3.0, 0.0);
        assert!(into > 3.0 + battery.standby.0 / 1000.0, "{into}");
        assert_eq!(out, 0.0);
        let (into, out) = ac_from_dc(&battery, 0.0, 2.0);
        assert!((into - battery.standby.0 / 1000.0).abs() < 1e-9);
        assert!(out < 2.0 && out > 1.8, "{out}");
    }

    #[test]
    fn money_compares_with_no_battery() {
        let config: Config = toml::from_str(include_str!("../../../dess.example.toml")).unwrap();
        let tariff = config.tariff.unwrap().to_tariff().unwrap();
        let slot = Slot::containing("2026-09-27T20:00:00Z".parse().unwrap());
        let spot = BTreeMap::from([(slot, EurPerKwh(0.10))]);
        let buy = tariff.prices(slot, EurPerKwh(0.10)).unwrap().buy.0;
        // 2 kWh load, 0.5 kWh PV; the battery covered 0.5 kWh of it.
        let flows = [SlotFlows {
            slot,
            covered_seconds: 900.0,
            import: WattHours(1000.0),
            export: WattHours(0.0),
            load: WattHours(2000.0),
            pv: WattHours(500.0),
            soc_start: 50.0,
            soc_end: 50.0,
            load_input: WattHours(0.0),
            relay_closed_seconds: [0.0, 0.0],
        }];
        let m = money(&flows, &spot, &tariff);
        assert!((m.actual - buy).abs() < 1e-9);
        assert!((m.without_battery - 1.5 * buy).abs() < 1e-9);
        assert!((m.hours - 0.25).abs() < 1e-9);
        // No price, no money.
        assert_eq!(money(&flows, &BTreeMap::new(), &tariff), Money::default());
    }

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
            battery_dc_in: Some("sensor.bat_in".into()),
            battery_dc_out: Some("sensor.bat_out".into()),
            heat_pump: None,
            ev: None,
        };
        let from = Slot::containing("2026-09-26T00:00:00Z".parse().unwrap());
        let loads = load_history(&store, &history, false, None, from).unwrap();
        assert_eq!(loads.len(), 4);
        // 1.0 − 0.2 + 1.5 − 0.8 + 0.1 = 1.6 kWh in an hour (no battery
        // model, so DC counts as AC).
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
            load_history(&store, &history, false, None, from)
                .unwrap()
                .len(),
            4
        );
    }
}
