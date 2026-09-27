//! Replays recorded history through the planner and the policy
//! (docs/DESIGN.md §7.8): what dess-oxide would have done, and what it
//! would have cost, with the load, PV and prices that actually happened.
//!
//! Each slot is planned from the forecasts dess-oxide had at its start, then
//! run at the slot's measured load and PV the way the per-second policy
//! would (at slot resolution). Perfect foresight plans the whole stretch
//! knowing what happened: no strategy can do better, so it bounds what
//! better forecasts could still gain.

use crate::battery::BatteryModel;
use crate::control::{self, Measured};
use crate::planner::{self, PlanRequest, PlannerSettings, SlotForecast};
use crate::slot::Slot;
use crate::tariff::SlotPrices;
use crate::units::Watts;

/// One recorded slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaySlot {
    pub slot: Slot,
    /// What happened: mean power over the slot.
    pub load: Watts,
    pub pv: Watts,
    pub prices: SlotPrices,
    /// The forecasts planned with at the start of this slot, from this slot
    /// on. `None` keeps following the previous plan.
    pub forecasts: Option<Vec<SlotForecast>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome {
    /// Import at the buy price minus export at the sell price, €.
    pub cost: f64,
    pub soc_end: f64,
}

/// Slots are replayed only when they follow each other; a gap stops it.
fn contiguous(history: &[ReplaySlot]) -> &[ReplaySlot] {
    let end = history
        .windows(2)
        .position(|w| w[1].slot != w[0].slot.next())
        .map_or(history.len(), |i| i + 1);
    &history[..end]
}

/// dess-oxide's policy over `history`, from `start_soc`.
pub fn replay(
    history: &[ReplaySlot],
    battery: &BatteryModel,
    settings: &PlannerSettings,
    min_soc_pct: f64,
    start_soc: f64,
) -> Outcome {
    let capacity = battery.capacity.0;
    let mut soc = start_soc;
    let mut pv_on = true;
    let mut plan = None;
    let mut cost = 0.0;
    for h in contiguous(history) {
        if let Some(forecasts) = &h.forecasts {
            plan = Some(planner::plan(&PlanRequest {
                now: h.slot.start(),
                soc_pct: soc,
                pv_on,
                battery,
                slots: forecasts,
                settings,
            }));
        }
        let planned = plan
            .as_ref()
            .and_then(|p| p.slots.iter().find(|s| s.slot == h.slot));
        let planned_pv = planned.is_none_or(|s| s.pv_on);
        // A bypass slot runs in bypass whatever happens, as the executor
        // does; the others follow the policy.
        let bypass = planned.filter(|s| s.bypass).and(battery.bypass_draw);
        let pv = if planned_pv { h.pv } else { Watts::ZERO };
        let measured = Measured {
            load: h.load,
            pv,
            soc_pct: soc,
        };
        let ac = plan
            .as_ref()
            .filter(|_| bypass.is_none())
            .and_then(|p| {
                control::decide(p, battery, settings, min_soc_pct, h.slot.start(), measured)
            })
            .map_or(0.0, |d| d.battery_ac.0);
        let hours = 0.25;
        let energy =
            (soc / 100.0 * capacity + battery.dc_for_ac(Watts(ac)).0 * hours).clamp(0.0, capacity);
        let grid = h.load.0 + bypass.unwrap_or(battery.standby).0 + ac - pv.0;
        cost +=
            (h.prices.buy.0 * grid.max(0.0) - h.prices.sell.0 * (-grid).max(0.0)) * hours / 1000.0;
        soc = energy / capacity * 100.0;
        pv_on = planned_pv;
    }
    Outcome { cost, soc_end: soc }
}

/// The best possible operation over `history`, knowing it in advance.
pub fn perfect_foresight(
    history: &[ReplaySlot],
    battery: &BatteryModel,
    settings: &PlannerSettings,
    min_soc_pct: f64,
    start_soc: f64,
) -> Outcome {
    let history = contiguous(history);
    let Some(first) = history.first() else {
        return Outcome {
            cost: 0.0,
            soc_end: start_soc,
        };
    };
    let forecasts: Vec<SlotForecast> = history
        .iter()
        .map(|h| SlotForecast {
            slot: h.slot,
            load: h.load,
            pv: h.pv,
            prices: h.prices,
            min_soc_end: min_soc_pct,
            estimated_price: false,
            islanded: false,
        })
        .collect();
    let plan = planner::plan(&PlanRequest {
        now: first.slot.start(),
        soc_pct: start_soc,
        pv_on: true,
        battery,
        slots: &forecasts,
        settings,
    });
    let cost = plan
        .slots
        .iter()
        .map(|s| {
            let grid = s.grid.0;
            (s.prices.buy.0 * grid.max(0.0) - s.prices.sell.0 * (-grid).max(0.0)) * s.hours / 1000.0
        })
        .sum();
    Outcome {
        cost,
        soc_end: plan.slots.last().map_or(start_soc, |s| s.soc_end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{EurPerKwh, WattHours};

    fn battery() -> BatteryModel {
        BatteryModel::multiplus_ii_prior(WattHours(10_000.0), 3, Watts(9000.0), Watts(12_000.0))
    }

    /// A day of cheap nights and expensive evenings, 1 kW load, no PV.
    fn history(forecast_load: f64) -> Vec<ReplaySlot> {
        let start = Slot::containing("2026-09-27T00:00:00Z".parse().unwrap());
        let slots: Vec<Slot> = std::iter::successors(Some(start), |s| Some(s.next()))
            .take(96)
            .collect();
        let price = |i: usize| {
            let buy = if (68..84).contains(&i) { 0.45 } else { 0.15 };
            SlotPrices {
                buy: EurPerKwh(buy),
                sell: EurPerKwh(buy - 0.05),
            }
        };
        let forecast = |from: usize| -> Vec<SlotForecast> {
            (from..96)
                .map(|i| SlotForecast {
                    slot: slots[i],
                    load: Watts(forecast_load),
                    pv: Watts::ZERO,
                    prices: price(i),
                    min_soc_end: 10.0,
                    estimated_price: false,
                    islanded: false,
                })
                .collect()
        };
        (0..96)
            .map(|i| ReplaySlot {
                slot: slots[i],
                load: Watts(1000.0),
                pv: Watts::ZERO,
                prices: price(i),
                forecasts: Some(forecast(i)),
            })
            .collect()
    }

    fn settings() -> PlannerSettings {
        PlannerSettings {
            terminal_value: EurPerKwh(0.10),
            ..PlannerSettings::default()
        }
    }

    #[test]
    fn replay_shifts_load_into_cheap_hours() {
        let h = history(1000.0);
        let (b, s) = (battery(), settings());
        let replayed = replay(&h, &b, &s, 10.0, 50.0);
        let perfect = perfect_foresight(&h, &b, &s, 10.0, 50.0);
        // Without the battery: 24 kWh, 4 of them at the evening price.
        let without = 20.0 * 0.15 + 4.0 * 0.45;
        let stored = |o: Outcome| (o.soc_end - 50.0) / 100.0 * 10.0 * 0.10;
        let net = |o: Outcome| o.cost - stored(o);
        assert!(net(replayed) < without - 0.5, "{replayed:?} vs {without}");
        // With perfect forecasts, the replay is about as good as foresight.
        assert!(
            net(perfect) <= net(replayed) + 0.05,
            "{perfect:?} {replayed:?}"
        );
    }

    #[test]
    fn a_gap_ends_the_replay() {
        let mut h = history(1000.0);
        h.remove(10);
        let replayed = replay(&h[..20], &battery(), &settings(), 10.0, 50.0);
        let first_ten = replay(&h[..10], &battery(), &settings(), 10.0, 50.0);
        assert_eq!(replayed, first_ten);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::units::{EurPerKwh, WattHours};

    #[test]
    #[ignore = "timing only"]
    fn a_week_at_full_size() {
        let battery = BatteryModel::multiplus_ii_prior(
            WattHours(32_000.0),
            3,
            Watts(10_000.0),
            Watts(12_000.0),
        );
        let start = Slot::containing("2026-09-20T00:00:00Z".parse().unwrap());
        let slots: Vec<Slot> = std::iter::successors(Some(start), |s| Some(s.next()))
            .take(672 + 192)
            .collect();
        let price = |i: usize| {
            let buy = 0.2 + 0.15 * ((i as f64) / 96.0 * std::f64::consts::TAU).sin();
            crate::tariff::SlotPrices {
                buy: EurPerKwh(buy),
                sell: EurPerKwh(buy - 0.03),
            }
        };
        let history: Vec<ReplaySlot> = (0..672)
            .map(|i| ReplaySlot {
                slot: slots[i],
                load: Watts(800.0),
                pv: Watts(if (40..64).contains(&(i % 96)) {
                    4000.0
                } else {
                    0.0
                }),
                prices: price(i),
                forecasts: Some(
                    (i..i + 192)
                        .map(|j| SlotForecast {
                            slot: slots[j],
                            load: Watts(800.0),
                            pv: Watts(if (40..64).contains(&(j % 96)) {
                                4000.0
                            } else {
                                0.0
                            }),
                            prices: price(j),
                            min_soc_end: 10.0,
                            estimated_price: false,
                            islanded: false,
                        })
                        .collect(),
                ),
            })
            .collect();
        let settings = PlannerSettings {
            pv_switchable: true,
            ..PlannerSettings::default()
        };
        let t = std::time::Instant::now();
        let r = replay(&history, &battery, &settings, 10.0, 50.0);
        let replay_s = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let p = perfect_foresight(&history, &battery, &settings, 10.0, 50.0);
        println!(
            "replay {replay_s:.1} s {r:?}; perfect {:.1} s {p:?}",
            t.elapsed().as_secs_f64()
        );
    }
}
