//! Forecasts against what happened, per day (docs/DESIGN.md §7.8): what the last
//! plan before midnight expected for the day, and what the recordings show.
//! Base load and heat pump are split where the heat pump model made the
//! forecast and its meter's hourly statistics are in.

use std::collections::HashMap;

use dess_core::{Slot, WattHours};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

use crate::config::Config;
use crate::store::{SlotFlows, Store};

/// Forecast and actual energy, kWh.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pair {
    pub forecast: f64,
    pub actual: f64,
}

impl Pair {
    fn add(&mut self, forecast: f64, actual: f64) {
        self.forecast += forecast;
        self.actual += actual;
    }

    /// Forecast error as a share of the actual.
    pub fn error(&self) -> Option<f64> {
        (self.actual.abs() > 0.05).then(|| (self.forecast - self.actual) / self.actual)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub date: Date,
    /// Hours compared (recorded, with a forecast).
    pub hours: f64,
    pub pv: Pair,
    pub load: Pair,
    /// Where the heat pump was forecast and metered.
    pub base: Option<Pair>,
    pub heat_pump: Option<Pair>,
}

/// Today (so far) and the `days` before it, newest first.
pub fn daily(
    store: &Store,
    config: &Config,
    tz: &TimeZone,
    now: Timestamp,
    days: i64,
) -> anyhow::Result<Vec<Day>> {
    let today = now.to_zoned(tz.clone()).date();
    let first = today.checked_sub(days.days())?;
    let start_of =
        |date: Date| -> anyhow::Result<Timestamp> { Ok(date.to_zoned(tz.clone())?.timestamp()) };
    let from = Slot::containing(start_of(first)?);
    let flows: HashMap<Slot, SlotFlows> = store
        .flows(from)?
        .into_iter()
        .filter(|f| f.covered_seconds >= 450.0 && f.slot.end() <= now)
        .map(|f| (f.slot, f))
        .collect();
    let heat_pump_entity = config
        .history
        .heat_pump
        .as_deref()
        .filter(|e| !e.is_empty());
    let heat_pump_hourly: HashMap<i64, f64> = match heat_pump_entity {
        Some(entity) => store
            .ha_hourly(&[entity], from.start())?
            .into_iter()
            .filter_map(|(hour, values)| Some((hour, *values.get(entity)?)))
            .filter(|(_, kwh)| !crate::planning::implausible(*kwh))
            .collect(),
        None => HashMap::new(),
    };
    let relay = config.pv_recorded_state();
    let kwh = |watts: f64| watts * 0.25 / 1000.0;

    let mut out = Vec::new();
    let mut date = today;
    while date >= first {
        let (start, end) = (start_of(date)?, start_of(date.tomorrow()?)?);
        let mut day = Day {
            date,
            hours: 0.0,
            pv: Pair::default(),
            load: Pair::default(),
            base: None,
            heat_pump: None,
        };
        for f in store.forecasts_made_before(Slot::containing(start), Slot::containing(end))? {
            let Some(flow) = flows.get(&f.slot) else {
                continue;
            };
            day.hours += 0.25;
            let ev = if config.ev.on_input {
                flow.load_input
            } else {
                WattHours(0.0)
            };
            let load = flow.mean(WattHours(flow.load.0 - ev.0)).0;
            day.load.add(kwh(f.load.0), kwh(load));
            if flow.pv_on_throughout(relay) {
                day.pv.add(kwh(f.pv.0), kwh(flow.mean(flow.pv).0));
            }
            let hour = f.slot.start_unix().div_euclid(3600) * 3600;
            if let (Some(forecast), Some(&metered)) = (f.heat_pump, heat_pump_hourly.get(&hour)) {
                let actual = metered / 4.0;
                day.heat_pump
                    .get_or_insert_default()
                    .add(kwh(forecast.0), actual);
                day.base
                    .get_or_insert_default()
                    .add(kwh(f.load.0 - forecast.0), kwh(load) - actual);
            }
        }
        if day.hours > 0.0 {
            out.push(day);
        }
        date = date.yesterday()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dess_core::battery::BatteryModel;
    use dess_core::planner::{self, PlanRequest, PlannerSettings, SlotForecast};
    use dess_core::record::{Recorder, Sample};
    use dess_core::tariff::SlotPrices;
    use dess_core::{EurPerKwh, Watts};
    use jiff::SignedDuration;

    #[test]
    fn splits_base_load_and_heat_pump_per_day() {
        let mut store = Store::in_memory().unwrap();
        let config: Config =
            toml::from_str("[victron]\nhost = \"x\"\n[history]\nheat_pump = \"sensor.hp\"\n")
                .unwrap();
        let midnight: Timestamp = "2026-09-27T00:00:00Z".parse().unwrap();

        // Planned at 23:00 the day before: 1 kW house load, 0.4 kW of it heat pump.
        let slots: Vec<Slot> =
            std::iter::successors(Some(Slot::containing(midnight)), |s| Some(s.next()))
                .take(4)
                .collect();
        let forecasts: Vec<SlotForecast> = slots
            .iter()
            .map(|&slot| SlotForecast {
                slot,
                load: Watts(1000.0),
                pv: Watts::ZERO,
                prices: SlotPrices {
                    buy: EurPerKwh(0.2),
                    sell: EurPerKwh(0.1),
                },
                min_soc_end: 0.0,
                estimated_price: false,
                islanded: false,
            })
            .collect();
        let battery = BatteryModel::multiplus_ii_prior(
            WattHours(10_000.0),
            3,
            Watts(9000.0),
            Watts(12_000.0),
        );
        let plan = planner::plan(&PlanRequest {
            now: midnight,
            soc_pct: 50.0,
            pv_on: true,
            battery: &battery,
            slots: &forecasts,
            settings: &PlannerSettings::default(),
        });
        let heat_pump = vec![Some(Watts(400.0)); 4];
        let planned_at = (midnight - SignedDuration::from_hours(1)).as_second();
        store
            .save_plan(planned_at, &plan.slots, &forecasts, &heat_pump)
            .unwrap();

        // What happened: 1.2 kW, 0.5 kWh of it the heat pump.
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        for s in 0..=3600 {
            let records = recorder.push(Sample {
                at: midnight + SignedDuration::from_secs(s),
                soc_pct: 50.0,
                battery: Watts::ZERO,
                battery_voltage: 52.0,
                grid: Watts(1200.0),
                pv_ac: Watts::ZERO,
                pv_dc: Watts::ZERO,
                load_out: Watts(1200.0),
                load_in: Watts::ZERO,
                inverter_ac: Watts::ZERO,
                grid_connected: true,
                relays: [None, None],
                setpoint: None,
            });
            for record in records {
                store.save_slot(&record, 0).unwrap();
            }
        }
        store
            .save_ha_hourly(&[("sensor.hp".to_owned(), midnight, 0.5)])
            .unwrap();

        let now = midnight + SignedDuration::from_hours(2);
        let days = daily(&store, &config, &TimeZone::UTC, now, 1).unwrap();
        assert_eq!(days.len(), 1);
        let day = &days[0];
        assert!((day.hours - 1.0).abs() < 1e-9);
        let close =
            |p: Pair, f: f64, a: f64| (p.forecast - f).abs() < 0.01 && (p.actual - a).abs() < 0.01;
        assert!(close(day.load, 1.0, 1.2), "{:?}", day.load);
        assert!(
            close(day.heat_pump.unwrap(), 0.4, 0.5),
            "{:?}",
            day.heat_pump
        );
        assert!(close(day.base.unwrap(), 0.6, 0.7), "{:?}", day.base);
    }
}
