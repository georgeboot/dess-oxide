//! The rollout comparison (docs/DESIGN.md §7.8): what actually happened (DAO's
//! result while it's in control) against dess-oxide's policy replayed over
//! the same loads, PV and prices, perfect foresight, and no battery.
//!
//! Each is net of the change in stored energy, valued at the planner's
//! terminal value, so ending the week with a fuller battery isn't a loss.

use std::collections::BTreeMap;

use dess_core::planner::SlotForecast;
use dess_core::replay::{self, ReplaySlot};
use dess_core::tariff::Tariff;
use dess_core::{EurPerKwh, Slot};
use jiff::{SignedDuration, Timestamp};

use crate::planning::PlanView;
use crate::store::{SlotFlows, Store};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Comparison {
    pub made_at: Option<Timestamp>,
    /// Hours compared.
    pub hours: f64,
    /// What happened.
    pub actual: f64,
    /// dess-oxide's plans and policy, with its forecasts at the time.
    pub replayed: f64,
    /// Knowing the loads, PV and prices in advance.
    pub perfect: f64,
    pub without_battery: f64,
}

/// What the comparison reads from the store.
pub struct Inputs {
    flows: Vec<SlotFlows>,
    spot: BTreeMap<Slot, EurPerKwh>,
    forecasts: BTreeMap<Slot, Vec<SlotForecast>>,
}

/// Reads the last `days` of recordings, prices and plans.
pub fn gather(store: &Store, now: Timestamp, days: i64) -> anyhow::Result<Inputs> {
    let from = Slot::containing(now - SignedDuration::from_hours(24 * days));
    Ok(Inputs {
        flows: store.flows(from)?,
        spot: store.prices(from, Slot::containing(now).next())?,
        forecasts: store.plan_forecasts(from)?,
    })
}

/// Compares the recorded slots that have plans. The replay takes a while:
/// call it without holding the store.
pub fn compare(inputs: Inputs, view: &PlanView, tariff: &Tariff, now: Timestamp) -> Comparison {
    let Inputs {
        flows,
        spot,
        mut forecasts,
    } = inputs;
    // Only from the first plan on: before it, there's nothing to replay.
    let first_plan = forecasts.keys().next().copied();

    let mut segments: Vec<Vec<(SlotFlows, ReplaySlot)>> = Vec::new();
    for f in flows {
        if f.slot.end() > now || first_plan.is_none_or(|first| f.slot < first) {
            continue;
        }
        let Some(prices) = spot
            .get(&f.slot)
            .and_then(|&s| tariff.prices(f.slot, s).ok())
        else {
            continue;
        };
        let replay_slot = ReplaySlot {
            slot: f.slot,
            load: f.mean(f.load),
            pv: f.mean(f.pv),
            prices,
            forecasts: forecasts.remove(&f.slot).map(|mut plan| {
                for slot in &mut plan {
                    slot.min_soc_end = view.min_soc;
                }
                plan
            }),
        };
        let continues = segments
            .last()
            .and_then(|s| s.last())
            .is_some_and(|(last, _)| last.slot.next() == f.slot);
        if !continues {
            segments.push(Vec::new());
        }
        if let Some(segment) = segments.last_mut() {
            segment.push((f, replay_slot));
        }
    }

    let capacity_kwh = view.battery.capacity.0 / 1000.0;
    let stored = |from_soc: f64, to_soc: f64| {
        (to_soc - from_soc) / 100.0 * capacity_kwh * view.settings.terminal_value.0
    };
    let mut result = Comparison {
        made_at: Some(now),
        ..Comparison::default()
    };
    for segment in segments {
        let (Some((first, _)), Some((last, _))) = (segment.first(), segment.last()) else {
            continue;
        };
        let start_soc = first.soc_start;
        for (f, r) in &segment {
            let kwh = |wh: dess_core::WattHours| wh.0 / 1000.0;
            let (buy, sell) = (r.prices.buy.0, r.prices.sell.0);
            // Scaled to the whole slot, like the replay's mean powers.
            let scale = 900.0 / f.covered_seconds.max(1.0);
            result.actual += (kwh(f.import) * buy - kwh(f.export) * sell) * scale;
            let net = (kwh(f.load) - kwh(f.pv)) * scale;
            result.without_battery += net.max(0.0) * buy - (-net).max(0.0) * sell;
            result.hours += 0.25;
        }
        result.actual -= stored(start_soc, last.soc_end);
        let slots: Vec<ReplaySlot> = segment.into_iter().map(|(_, r)| r).collect();
        let (battery, settings) = (&view.battery, &view.settings);
        let replayed = replay::replay(&slots, battery, settings, view.min_soc, start_soc);
        result.replayed += replayed.cost - stored(start_soc, replayed.soc_end);
        let perfect = replay::perfect_foresight(&slots, battery, settings, view.min_soc, start_soc);
        result.perfect += perfect.cost - stored(start_soc, perfect.soc_end);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use dess_core::battery::BatteryModel;
    use dess_core::planner::{self, PlanRequest, PlannerSettings};
    use dess_core::record::{Recorder, Sample};
    use dess_core::{EurPerKwh, WattHours, Watts};

    /// Four hours at 2 kW with the battery idle at 60 %, recorded.
    fn record_idle(store: &mut Store, start: Timestamp) {
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        for s in 0..=4 * 3600 {
            let records = recorder.push(Sample {
                at: start + SignedDuration::from_secs(s),
                soc_pct: 60.0,
                battery: Watts::ZERO,
                battery_voltage: 52.0,
                grid: Watts(2000.0),
                pv_ac: Watts::ZERO,
                pv_dc: Watts::ZERO,
                load_out: Watts(2000.0),
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
    }

    /// Four hours: two cheap, two expensive. The house uses 2 kW, and DAO
    /// left the battery idle at 60 %.
    #[test]
    fn replays_a_recorded_stretch() {
        let mut store = Store::in_memory().unwrap();
        let start: Timestamp = "2026-09-27T16:00:00Z".parse().unwrap();
        record_idle(&mut store, start);
        let slots: Vec<Slot> =
            std::iter::successors(Some(Slot::containing(start)), |s| Some(s.next()))
                .take(16)
                .collect();
        let spot = |i: usize| if i < 8 { 50.0 } else { 400.0 };
        let prices: Vec<(Slot, f64)> = slots
            .iter()
            .enumerate()
            .map(|(i, &s)| (s, spot(i)))
            .collect();
        store.save_prices(&prices, true, "test", 0).unwrap();

        let tariff = {
            let config: crate::config::Config =
                toml::from_str(include_str!("../../../dess.example.toml")).unwrap();
            config.tariff.unwrap().to_tariff().unwrap()
        };
        let battery = BatteryModel::multiplus_ii_prior(
            WattHours(10_000.0),
            3,
            Watts(9000.0),
            Watts(12_000.0),
        );
        let settings = PlannerSettings {
            terminal_value: EurPerKwh(0.05),
            ..PlannerSettings::default()
        };
        let forecast = |from: usize| -> Vec<SlotForecast> {
            (from..16)
                .map(|i| SlotForecast {
                    slot: slots[i],
                    load: Watts(2000.0),
                    pv: Watts::ZERO,
                    prices: tariff
                        .prices(slots[i], EurPerKwh::from_eur_per_mwh(spot(i)))
                        .unwrap(),
                    min_soc_end: 10.0,
                    estimated_price: false,
                    islanded: false,
                })
                .collect()
        };
        let plan_at = |i: usize| {
            let forecasts = forecast(i);
            let plan = planner::plan(&PlanRequest {
                now: slots[i].start(),
                soc_pct: 60.0,
                pv_on: true,
                battery: &battery,
                slots: &forecasts,
                settings: &settings,
            });
            (forecasts, plan)
        };
        for (i, slot) in slots.iter().enumerate() {
            let (forecasts, plan) = plan_at(i);
            store
                .save_plan(slot.start_unix() + 5, &plan.slots, &forecasts, &[])
                .unwrap();
        }
        let (forecasts, plan) = plan_at(0);
        let view = PlanView {
            planned_at: start,
            soc: 60.0,
            battery,
            forecasts,
            heat_pump: Vec::new(),
            settings,
            min_soc: 10.0,
            limits: crate::planning::PowerLimits::default(),
            plan,
        };

        let now = start + SignedDuration::from_hours(5);
        let inputs = gather(&store, now, 1).unwrap();
        let c = compare(inputs, &view, &tariff, now);
        assert!((c.hours - 4.0).abs() < 1e-9, "{c:?}");
        // Idle, so what happened is the same as no battery.
        assert!((c.actual - c.without_battery).abs() < 0.01, "{c:?}");
        // dess-oxide would have covered the expensive hours from the battery.
        assert!(c.replayed < c.actual - 0.3, "{c:?}");
        assert!(c.perfect <= c.replayed + 0.02, "{c:?}");
    }
}
