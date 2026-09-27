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
use crate::planner::{Plan, PlannerSettings};
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
    let planned = &plan.slots[t];
    let hours = (current.remaining(now).as_secs_f64() / 3600.0).max(1.0 / 3600.0);
    let capacity = battery.capacity.0;
    let energy = measured.soc_pct / 100.0 * capacity;
    let floor = min_soc_pct / 100.0 * capacity;
    let prices = planned.prices;

    let cost = |ac: f64| {
        let dc = battery.dc_for_ac(Watts(ac)).0;
        let end = (energy + dc * hours).clamp(0.0, capacity);
        let grid = measured.load.0 + battery.standby.0 + ac - measured.pv.0;
        let (import, export) = (grid.max(0.0), (-grid).max(0.0));
        let excess =
            (import - settings.max_import.0).max(0.0) + (export - settings.max_export.0).max(0.0);
        let shortfall = (floor - end).max(0.0);
        (prices.buy.0 * import - prices.sell.0 * export) * hours / 1000.0
            + settings.wear_cost.0 * dc.abs() * hours / 1000.0
            + settings.grid_excess_penalty.0 * excess * hours / 1000.0
            + settings.shortfall_penalty.0 * shortfall / 1000.0
            + plan.cost_to_go(t + 1, WattHours(end), planned.pv_on)
    };

    // Only powers that stay within the battery's range this slot.
    let max_charge = battery
        .max_charge_ac
        .0
        .min(((capacity - energy) / hours).max(0.0) * 1.1);
    let max_discharge = battery.max_discharge_ac.0.min((energy / hours).max(0.0));
    let steps = |limit: f64| (limit / STEP_W).floor() as i64;
    let (best, _) = (-steps(max_discharge)..=steps(max_charge))
        .map(|k| k as f64 * STEP_W)
        .map(|ac| (ac, cost(ac)))
        .fold((0.0, f64::INFINITY), |best, candidate| {
            if candidate.1 < best.1 - 1e-9 {
                candidate
            } else {
                best
            }
        });
    Some(Decision {
        battery_ac: Watts(best),
        setpoint: Watts(measured.load.0 - measured.pv.0 + best),
        pv_on: planned.pv_on,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{PlanRequest, SlotForecast, plan};
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
}
