//! The per-second decision: the plan's policy applied to what is actually
//! happening (PLAN.md §15.2).
//!
//! The plan's numbers are a forecast; its value function is the policy. Each
//! second, for the rest of the current slot, we pick the battery power that
//! minimises this slot's cost at the *measured* load and PV plus the plan's
//! cost-to-go for the energy left at the slot's end. Surprises then go where
//! they're cheapest: when exporting at a high price, extra load comes from the
//! battery (the export stays); when holding the battery for a later peak,
//! extra load comes from the grid.

use jiff::Timestamp;

use crate::battery::BatteryModel;
use crate::planner::{Plan, PlannerSettings, SlotForecast};
use crate::slot::Slot;
use crate::units::{WattHours, Watts};

/// Candidate battery powers are this far apart.
const STEP_W: f64 = 100.0;

/// What is happening right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// All loads.
    pub load: Watts,
    /// PV output as it is (zero when the relay has it off).
    pub pv: Watts,
    pub soc_pct: f64,
}

/// What to do for the rest of the slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Inverter/charger AC power, positive = charging.
    pub battery_ac: Watts,
    /// The ESS grid setpoint that produces it, positive = import.
    pub setpoint: Watts,
    /// Whether PV should be on in this slot (the plan's choice).
    pub pv_on: bool,
}

/// The best battery power now, or `None` if `now` isn't in the plan.
pub fn decide(
    plan: &Plan,
    battery: &BatteryModel,
    settings: &PlannerSettings,
    min_soc_pct: f64,
    now: Timestamp,
    measured: Measured,
) -> Option<Decision> {
    let current = Slot::containing(now);
    let t = plan.slots.iter().position(|s| s.slot == current)?;
    let hours = (current.remaining(now).as_secs_f64() / 3600.0).max(1.0 / 3600.0);
    let stage = Stage {
        plan,
        battery,
        settings,
        floor: min_soc_pct / 100.0 * battery.capacity.0,
        t,
        hours,
    };
    let energy = measured.soc_pct / 100.0 * battery.capacity.0;
    let (best, _) = stage.best(energy, measured.load.0, measured.pv.0);
    Some(Decision {
        battery_ac: Watts(best),
        setpoint: Watts(measured.load.0 - measured.pv.0 + best),
        pv_on: plan.slots[t].pv_on,
    })
}

/// What extra load would cost in each of the plan's slots, €/kWh, with the
/// battery responding as the policy would: the buy price where the grid
/// covers it, the stored energy's value where the battery does, the lost
/// sell price where it cuts an export. `forecasts` are the plan's inputs.
/// Islanded slots are infinitely expensive.
pub fn marginal_costs(
    plan: &Plan,
    forecasts: &[SlotForecast],
    battery: &BatteryModel,
    settings: &PlannerSettings,
    min_soc_pct: f64,
    extra: Watts,
) -> Vec<f64> {
    let capacity = battery.capacity.0;
    plan.slots
        .iter()
        .enumerate()
        .map(|(t, planned)| {
            let Some(forecast) = forecasts.iter().find(|f| f.slot == planned.slot) else {
                return f64::INFINITY;
            };
            if forecast.islanded {
                return f64::INFINITY;
            }
            let stage = Stage {
                plan,
                battery,
                settings,
                floor: min_soc_pct / 100.0 * capacity,
                t,
                hours: planned.hours.max(1.0 / 3600.0),
            };
            let energy = planned.soc_start / 100.0 * capacity;
            let pv = if planned.pv_on { forecast.pv.0 } else { 0.0 };
            let (_, base) = stage.best(energy, forecast.load.0, pv);
            let (_, with) = stage.best(energy, forecast.load.0 + extra.0, pv);
            (with - base) / (extra.0 * stage.hours / 1000.0)
        })
        .collect()
}

/// One slot of the plan, from some point in it to its end.
struct Stage<'a> {
    plan: &'a Plan,
    battery: &'a BatteryModel,
    settings: &'a PlannerSettings,
    /// The SoC floor, Wh.
    floor: f64,
    t: usize,
    hours: f64,
}

impl Stage<'_> {
    /// The cheapest battery AC power at a constant load and PV, starting at
    /// `energy` Wh, and its cost including the plan's cost-to-go.
    fn best(&self, energy: f64, load: f64, pv: f64) -> (f64, f64) {
        let (battery, settings, hours) = (self.battery, self.settings, self.hours);
        let capacity = battery.capacity.0;
        let planned = &self.plan.slots[self.t];
        let prices = planned.prices;
        let cost = |ac: f64| {
            let dc = battery.dc_for_ac(Watts(ac)).0;
            let end = (energy + dc * hours).clamp(0.0, capacity);
            let grid = load + battery.standby.0 + ac - pv;
            let (import, export) = (grid.max(0.0), (-grid).max(0.0));
            let excess = (import - settings.max_import.0).max(0.0)
                + (export - settings.max_export.0).max(0.0);
            let shortfall = (self.floor - end).max(0.0);
            (prices.buy.0 * import - prices.sell.0 * export) * hours / 1000.0
                + settings.wear_cost.0 * dc.abs() * hours / 1000.0
                + settings.grid_excess_penalty.0 * excess * hours / 1000.0
                + settings.shortfall_penalty.0 * shortfall / 1000.0
                + self
                    .plan
                    .cost_to_go(self.t + 1, WattHours(end), planned.pv_on)
        };

        // Only powers that stay within the battery's range this slot.
        let max_charge = battery
            .max_charge_ac
            .0
            .min(((capacity - energy) / hours).max(0.0) * 1.1);
        let max_discharge = battery.max_discharge_ac.0.min((energy / hours).max(0.0));
        let steps = |limit: f64| (limit / STEP_W).floor() as i64;
        (-steps(max_discharge)..=steps(max_charge))
            .map(|k| k as f64 * STEP_W)
            .map(|ac| (ac, cost(ac)))
            .fold((0.0, f64::INFINITY), |best, candidate| {
                if candidate.1 < best.1 - 1e-9 {
                    candidate
                } else {
                    best
                }
            })
    }
}

/// The cheapest run of `slots_needed` consecutive slots that starts at or
/// after `earliest` and ends by `finish_by`: its first index and mean cost.
/// Ties go to the earliest start.
pub fn cheapest_run(
    slots: &[Slot],
    costs: &[f64],
    earliest: Timestamp,
    finish_by: Timestamp,
    slots_needed: usize,
) -> Option<(usize, f64)> {
    let n = slots_needed.max(1);
    (0..slots.len().min(costs.len()).saturating_sub(n - 1))
        .filter(|&i| slots[i].start() >= earliest && slots[i + n - 1].end() <= finish_by)
        .map(|i| (i, costs[i..i + n].iter().sum::<f64>() / n as f64))
        .filter(|(_, mean)| mean.is_finite())
        .fold(None, |best: Option<(usize, f64)>, candidate| match best {
            Some(b) if b.1 <= candidate.1 + 1e-9 => Some(b),
            _ => Some(candidate),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{PlanRequest, plan};
    use crate::tariff::SlotPrices;
    use crate::units::EurPerKwh;

    fn battery() -> BatteryModel {
        BatteryModel::multiplus_ii_prior(WattHours(10_000.0), 3, Watts(10_000.0), Watts(11_000.0))
    }

    fn settings() -> PlannerSettings {
        PlannerSettings {
            energy_step: WattHours(100.0),
            ..PlannerSettings::default()
        }
    }

    fn start() -> Timestamp {
        "2026-09-27T17:00:00Z".parse().unwrap()
    }

    fn make(prices: &[(f64, f64)], load: f64, soc: f64) -> Plan {
        make_with(prices, load, soc, &settings())
    }

    fn make_with(prices: &[(f64, f64)], load: f64, soc: f64, settings: &PlannerSettings) -> Plan {
        let mut slot = Slot::containing(start());
        let slots: Vec<SlotForecast> = prices
            .iter()
            .map(|&(buy, sell)| {
                let f = SlotForecast {
                    slot,
                    load: Watts(load),
                    pv: Watts::ZERO,
                    prices: SlotPrices {
                        buy: EurPerKwh(buy),
                        sell: EurPerKwh(sell),
                    },
                    min_soc_end: 0.0,
                    estimated_price: false,
                    islanded: false,
                };
                slot = slot.next();
                f
            })
            .collect();
        plan(&PlanRequest {
            now: start(),
            soc_pct: soc,
            pv_on: true,
            battery: &battery(),
            slots: &slots,
            settings,
        })
    }

    #[test]
    fn keeps_exporting_when_the_house_uses_more() {
        // A high price now and cheap ones later: the plan exports up to a
        // 5 kW export limit, with the battery below its own maximum.
        let limited = PlannerSettings {
            max_export: Watts(5000.0),
            ..settings()
        };
        let p = make_with(
            &[(0.60, 0.55), (0.10, 0.05), (0.10, 0.05), (0.10, 0.05)],
            500.0,
            80.0,
            &limited,
        );
        let planned_grid = p.slots[0].grid.0;
        assert!(
            (planned_grid + 5000.0).abs() < 300.0,
            "plan exports at the limit: {planned_grid}"
        );
        let now = start() + jiff::SignedDuration::from_mins(5);
        let surprise = Measured {
            load: Watts(2500.0),
            pv: Watts::ZERO,
            soc_pct: 78.0,
        };
        let d = decide(&p, &battery(), &limited, 0.0, now, surprise).unwrap();
        // The extra 2 kW comes from the battery; the export stays.
        assert!(
            (d.setpoint.0 - planned_grid).abs() < 300.0,
            "setpoint {} vs planned {planned_grid}",
            d.setpoint.0
        );
    }

    #[test]
    fn a_held_battery_leaves_surprises_to_the_grid() {
        // Cheap now, very expensive later: hold the battery for later.
        let p = make(
            &[(0.10, 0.05), (0.10, 0.05), (0.80, 0.70), (0.80, 0.70)],
            500.0,
            40.0,
        );
        let now = start() + jiff::SignedDuration::from_mins(5);
        let ev = Measured {
            load: Watts(7500.0),
            pv: Watts::ZERO,
            soc_pct: 40.0,
        };
        let d = decide(&p, &battery(), &settings(), 0.0, now, ev).unwrap();
        assert!(
            d.battery_ac.0 >= -100.0,
            "doesn't drain into the car: {d:?}"
        );
        assert!(d.setpoint.0 > 7000.0);
    }

    #[test]
    fn nothing_outside_the_plan() {
        let p = make(&[(0.1, 0.1)], 500.0, 50.0);
        let later = start() + jiff::SignedDuration::from_hours(3);
        let m = Measured {
            load: Watts(500.0),
            pv: Watts::ZERO,
            soc_pct: 50.0,
        };
        assert!(decide(&p, &battery(), &settings(), 0.0, later, m).is_none());
    }

    fn forecasts(prices: &[(f64, f64)], load: f64) -> Vec<SlotForecast> {
        let mut slot = Slot::containing(start());
        prices
            .iter()
            .map(|&(buy, sell)| {
                let f = SlotForecast {
                    slot,
                    load: Watts(load),
                    pv: Watts::ZERO,
                    prices: SlotPrices {
                        buy: EurPerKwh(buy),
                        sell: EurPerKwh(sell),
                    },
                    min_soc_end: 0.0,
                    estimated_price: false,
                    islanded: false,
                };
                slot = slot.next();
                f
            })
            .collect()
    }

    #[test]
    fn extra_load_costs_what_covers_it() {
        // An empty battery and a cheap night between two expensive
        // evenings: extra load costs the grid price where it lands.
        let prices = [
            (0.40, 0.30),
            (0.40, 0.30),
            (0.10, 0.05),
            (0.10, 0.05),
            (0.40, 0.30),
            (0.40, 0.30),
        ];
        let p = make(&prices, 500.0, 0.0);
        let costs = marginal_costs(
            &p,
            &forecasts(&prices, 500.0),
            &battery(),
            &settings(),
            0.0,
            Watts(1000.0),
        );
        assert!(costs[2] < 0.2 && costs[3] < 0.2, "{costs:?}");
        assert!(costs[0] > 0.3, "{costs:?}");
    }

    #[test]
    fn the_cheapest_run_fits_the_window() {
        let first = Slot::containing(start());
        let slots: Vec<Slot> = std::iter::successors(Some(first), |s| Some(s.next()))
            .take(8)
            .collect();
        let costs = [0.3, 0.1, 0.1, 0.2, 0.05, 0.05, 0.05, 0.4];
        let at = |i: usize| slots[i].start();
        // Two slots anywhere: 4 and 5.
        assert_eq!(cheapest_run(&slots, &costs, at(0), at(7), 2).unwrap().0, 4);
        // It must finish by slot 4's start: 1 and 2.
        assert_eq!(cheapest_run(&slots, &costs, at(0), at(4), 2).unwrap().0, 1);
        // Doesn't start before slot 2, and ends by slot 4's end: 3 and 4.
        assert_eq!(cheapest_run(&slots, &costs, at(2), at(5), 2).unwrap().0, 3);
        assert!(cheapest_run(&slots, &costs, at(6), at(7), 2).is_none());
        let mut islanded = costs;
        islanded[5] = f64::INFINITY;
        assert_eq!(
            cheapest_run(&slots, &islanded, at(0), slots[7].end(), 2)
                .unwrap()
                .0,
            1
        );
    }
}
