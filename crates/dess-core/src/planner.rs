//! The battery schedule: exact dynamic programming over stored energy.
//!
//! State: stored energy on a fixed grid (e.g. 100 Wh) × the PV relay state of
//! the previous slot. Decision per slot: the next energy level and whether PV
//! is on. The backward pass computes the cost-to-go `V_t(E, relay)` for every
//! state; the forward pass extracts the plan from the current SoC. Keeping the
//! whole value function lets the executor apply the plan's policy to measured
//! load and PV every second (PLAN.md §15.2).
//!
//! Minimum SoC and grid limits are soft (large penalties), so a plan always
//! exists, even when starting below the floor or when loads exceed what the
//! grid connection allows.

use jiff::Timestamp;

use crate::battery::BatteryModel;
use crate::slot::{SLOT_SECONDS, Slot};
use crate::tariff::SlotPrices;
use crate::units::{EurPerKwh, WattHours, Watts};

/// Forecast and prices for one slot.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotForecast {
    pub slot: Slot,
    /// Mean consumption of all loads.
    pub load: Watts,
    /// Mean PV production, if the PV is on.
    pub pv: Watts,
    pub prices: SlotPrices,
    /// Lowest SoC (%) allowed at the end of the slot.
    pub min_soc_end: f64,
    /// The price is an estimate beyond the published day-ahead prices.
    pub estimated_price: bool,
    /// An expected outage: no grid. Importing is impossible (penalised),
    /// surplus PV is curtailed for free, and PV stays on.
    pub islanded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannerSettings {
    /// Resolution of the stored-energy grid.
    pub energy_step: WattHours,
    pub max_import: Watts,
    pub max_export: Watts,
    /// Cost per kWh moved in or out of the battery (DC side).
    pub wear_cost: EurPerKwh,
    /// Whether a relay can switch the PV off.
    pub pv_switchable: bool,
    /// Cost of switching the PV relay, in euro; keeps it from chattering.
    pub relay_switch_cost: f64,
    /// Value of energy still stored at the end of the horizon.
    pub terminal_value: EurPerKwh,
    /// Penalty per kWh below a slot's minimum SoC.
    pub shortfall_penalty: EurPerKwh,
    /// Penalty per kWh of import or export beyond the grid limits.
    pub grid_excess_penalty: EurPerKwh,
}

impl Default for PlannerSettings {
    fn default() -> Self {
        Self {
            energy_step: WattHours(100.0),
            max_import: Watts(17_000.0),
            max_export: Watts(17_000.0),
            wear_cost: EurPerKwh::ZERO,
            pv_switchable: false,
            relay_switch_cost: 0.01,
            terminal_value: EurPerKwh::ZERO,
            shortfall_penalty: EurPerKwh(10.0),
            grid_excess_penalty: EurPerKwh(10.0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PlanRequest<'a> {
    pub now: Timestamp,
    pub soc_pct: f64,
    /// Whether PV is on right now.
    pub pv_on: bool,
    pub battery: &'a BatteryModel,
    /// Consecutive slots, the first containing `now`.
    pub slots: &'a [SlotForecast],
    pub settings: &'a PlannerSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedSlot {
    pub slot: Slot,
    /// Length of the planned part of the slot (the first one is partial).
    pub hours: f64,
    pub soc_start: f64,
    pub soc_end: f64,
    /// Inverter/charger AC power, positive = charging.
    pub battery_ac: Watts,
    /// Battery terminal power, positive = charging.
    pub battery_dc: Watts,
    /// Grid power, positive = import.
    pub grid: Watts,
    pub pv_on: bool,
    /// Grid cost plus wear for this slot, in euro.
    pub cost: f64,
    /// Marginal value of stored energy at the end of the slot.
    pub stored_energy_value: EurPerKwh,
    pub prices: SlotPrices,
    pub estimated_price: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub slots: Vec<PlannedSlot>,
    /// Cost over the horizon, minus the value of the energy left at the end.
    pub expected_cost: f64,
    values: ValueFunction,
}

impl Plan {
    /// Expected cost from the start of planned slot `t` onward when arriving
    /// there with `energy` stored and the PV relay as it was in the previous slot.
    pub fn cost_to_go(&self, t: usize, energy: WattHours, pv_on: bool) -> f64 {
        self.values.interpolate(t, usize::from(pv_on), energy.0)
    }
}

/// `V_t(level, relay)` for t in `0..=T`.
#[derive(Debug, Clone)]
struct ValueFunction {
    step_wh: f64,
    levels: usize,
    v: Vec<f64>,
}

impl ValueFunction {
    fn index(&self, t: usize, relay: usize, level: usize) -> usize {
        (t * 2 + relay) * self.levels + level
    }

    fn get(&self, t: usize, relay: usize, level: usize) -> f64 {
        self.v[self.index(t, relay, level)]
    }

    fn interpolate(&self, t: usize, relay: usize, energy_wh: f64) -> f64 {
        let x = (energy_wh / self.step_wh).clamp(0.0, (self.levels - 1) as f64);
        let lower = x.floor() as usize;
        let upper = (lower + 1).min(self.levels - 1);
        let w = x - lower as f64;
        self.get(t, relay, lower) * (1.0 - w) + self.get(t, relay, upper) * w
    }

    /// What the last stored step of energy at `level` is worth (€/kWh): how
    /// much the cost-to-go rises when it's used up. V has kinks exactly where
    /// plans change, so this is a one-sided difference, not a central one.
    fn marginal_value(&self, t: usize, relay: usize, level: usize) -> EurPerKwh {
        if level == 0 {
            return EurPerKwh::ZERO;
        }
        let rise = self.get(t, relay, level - 1) - self.get(t, relay, level);
        EurPerKwh(rise / self.step_wh * 1000.0)
    }
}

/// One allowed change of stored energy within a slot.
#[derive(Debug, Clone, Copy)]
struct Move {
    levels: isize,
    ac: f64,
    dc: f64,
}

pub fn plan(request: &PlanRequest<'_>) -> Plan {
    let problem = Problem::new(request);
    let (values, policy) = problem.backward();
    let start_level = problem.level_of(request.soc_pct);
    let start_relay = if request.settings.pv_switchable {
        usize::from(request.pv_on)
    } else {
        1
    };
    Plan {
        slots: problem.forward(&values, &policy, start_level, start_relay),
        expected_cost: values.get(0, start_relay, start_level),
        values,
    }
}

/// The best decision for one state: an energy move and the relay state.
#[derive(Debug, Clone, Copy)]
struct Decision {
    levels: isize,
    relay: usize,
}

/// A plan request, discretised.
struct Problem<'a> {
    request: &'a PlanRequest<'a>,
    step: f64,
    capacity: f64,
    levels: usize,
    /// Planned hours per slot; the first slot is partial.
    hours: Vec<f64>,
    first_slot_moves: Vec<Move>,
    full_slot_moves: Vec<Move>,
}

impl<'a> Problem<'a> {
    fn new(request: &'a PlanRequest<'a>) -> Self {
        let step = request.settings.energy_step.0;
        let capacity = request.battery.capacity.0;
        let full_slot = SLOT_SECONDS as f64 / 3600.0;
        let hours: Vec<f64> = request
            .slots
            .iter()
            .enumerate()
            .map(|(t, forecast)| {
                if t == 0 {
                    (forecast.slot.remaining(request.now).as_secs_f64() / 3600.0).max(1.0 / 3600.0)
                } else {
                    full_slot
                }
            })
            .collect();
        Self {
            request,
            step,
            capacity,
            levels: (capacity / step).floor() as usize + 1,
            first_slot_moves: moves(
                request.battery,
                step,
                hours.first().copied().unwrap_or(full_slot),
            ),
            full_slot_moves: moves(request.battery, step, full_slot),
            hours,
        }
    }

    fn moves(&self, t: usize) -> &[Move] {
        if t == 0 {
            &self.first_slot_moves
        } else {
            &self.full_slot_moves
        }
    }

    /// The grid level nearest to `soc_pct`.
    fn level_of(&self, soc_pct: f64) -> usize {
        ((soc_pct / 100.0 * self.capacity) / self.step)
            .round()
            .clamp(0.0, (self.levels - 1) as f64) as usize
    }

    fn soc_of(&self, level: usize) -> f64 {
        level as f64 * self.step / self.capacity * 100.0
    }

    /// Computes `V_t` for every state, and the decision that achieves it.
    fn backward(&self) -> (ValueFunction, Vec<Decision>) {
        let PlanRequest {
            battery,
            slots,
            settings,
            ..
        } = *self.request;
        let horizon = slots.len();
        let levels = self.levels;
        let switchable: &[usize] = if settings.pv_switchable {
            &[0, 1]
        } else {
            &[1]
        };

        let mut values = ValueFunction {
            step_wh: self.step,
            levels,
            v: vec![f64::INFINITY; (horizon + 1) * 2 * levels],
        };
        for relay in 0..2 {
            for level in 0..levels {
                let i = values.index(horizon, relay, level);
                values.v[i] = -settings.terminal_value.0 * level as f64 * self.step / 1000.0;
            }
        }
        let mut policy = vec![
            Decision {
                levels: 0,
                relay: 1
            };
            horizon * 2 * levels
        ];

        for t in (0..horizon).rev() {
            let forecast = &slots[t];
            let hours = self.hours[t];
            let moves = self.moves(t);
            let relays: &[usize] = if forecast.islanded { &[1] } else { switchable };
            // The lowest level that still meets the slot's minimum SoC.
            let floor_level = ((forecast.min_soc_end / 100.0 * self.capacity) / self.step)
                .ceil()
                .max(0.0) as usize;
            // The stage cost depends on the move, not on the starting level.
            let stage: Vec<[f64; 2]> = moves
                .iter()
                .map(|m| {
                    [0, 1]
                        .map(|relay| stage_cost(forecast, battery, settings, hours, m, relay == 1))
                })
                .collect();

            for previous_relay in 0..2 {
                for level in 0..levels {
                    let mut best = (
                        f64::INFINITY,
                        Decision {
                            levels: 0,
                            relay: previous_relay,
                        },
                    );
                    // Keeping the relay is tried first, so ties don't switch it.
                    for relay in [previous_relay, 1 - previous_relay] {
                        if !relays.contains(&relay) {
                            continue;
                        }
                        let switch = if relay == previous_relay {
                            0.0
                        } else {
                            settings.relay_switch_cost
                        };
                        for (m, costs) in moves.iter().zip(&stage) {
                            let Some(next) =
                                level.checked_add_signed(m.levels).filter(|&n| n < levels)
                            else {
                                continue;
                            };
                            let shortfall_wh = floor_level.saturating_sub(next) as f64 * self.step;
                            let total = costs[relay]
                                + switch
                                + settings.shortfall_penalty.0 * shortfall_wh / 1000.0
                                + values.get(t + 1, relay, next);
                            if total < best.0 - 1e-12 {
                                best = (
                                    total,
                                    Decision {
                                        levels: m.levels,
                                        relay,
                                    },
                                );
                            }
                        }
                    }
                    let i = values.index(t, previous_relay, level);
                    values.v[i] = best.0;
                    policy[i] = best.1;
                }
            }
        }
        (values, policy)
    }

    /// Follows the policy from the measured state.
    fn forward(
        &self,
        values: &ValueFunction,
        policy: &[Decision],
        mut level: usize,
        mut relay: usize,
    ) -> Vec<PlannedSlot> {
        let PlanRequest {
            battery,
            slots,
            settings,
            ..
        } = *self.request;
        let mut planned = Vec::with_capacity(slots.len());
        for (t, forecast) in slots.iter().enumerate() {
            let decision = policy[values.index(t, relay, level)];
            let hours = self.hours[t];
            let m = *self
                .moves(t)
                .iter()
                .find(|m| m.levels == decision.levels)
                .expect("the policy only picks allowed moves");
            let next = level
                .checked_add_signed(m.levels)
                .expect("the policy stays within the grid");
            let pv_on = decision.relay == 1;
            let grid = grid_power(forecast, battery, &m, pv_on);
            planned.push(PlannedSlot {
                slot: forecast.slot,
                hours,
                soc_start: self.soc_of(level),
                soc_end: self.soc_of(next),
                battery_ac: Watts(m.ac),
                battery_dc: Watts(m.dc),
                grid: Watts(grid),
                pv_on,
                cost: stage_cost(forecast, battery, settings, hours, &m, pv_on)
                    - excess_penalty(forecast, grid, settings, hours),
                stored_energy_value: values.marginal_value(t + 1, decision.relay, next),
                prices: forecast.prices,
                estimated_price: forecast.estimated_price,
            });
            level = next;
            relay = decision.relay;
        }
        planned
    }
}

/// Every energy change reachable within `hours`, with its AC and DC power.
fn moves(battery: &BatteryModel, step: f64, hours: f64) -> Vec<Move> {
    let reach = |power: f64| (power * 1.1 * hours / step).ceil() as isize;
    let up = reach(battery.max_charge_ac.0);
    let down = reach(battery.max_discharge_ac.0 * 1.2);
    (-down..=up)
        .filter_map(|levels| {
            let delta = WattHours(levels as f64 * step);
            battery.ac_power_for(delta, hours).map(|ac| Move {
                levels,
                ac: ac.0,
                dc: delta.0 / hours,
            })
        })
        .collect()
}

fn grid_power(forecast: &SlotForecast, battery: &BatteryModel, m: &Move, pv_on: bool) -> f64 {
    let pv = if pv_on { forecast.pv.0 } else { 0.0 };
    forecast.load.0 + battery.standby.0 + m.ac - pv
}

/// Penalty for grid use beyond what's possible: the connection's limits, or
/// any import at all while islanded.
fn excess_penalty(
    forecast: &SlotForecast,
    grid: f64,
    settings: &PlannerSettings,
    hours: f64,
) -> f64 {
    if forecast.islanded {
        return settings.shortfall_penalty.0 * grid.max(0.0) * hours / 1000.0;
    }
    let excess = (grid - settings.max_import.0).max(0.0) + (-grid - settings.max_export.0).max(0.0);
    settings.grid_excess_penalty.0 * excess * hours / 1000.0
}

fn stage_cost(
    forecast: &SlotForecast,
    battery: &BatteryModel,
    settings: &PlannerSettings,
    hours: f64,
    m: &Move,
    pv_on: bool,
) -> f64 {
    let grid = grid_power(forecast, battery, m, pv_on);
    let wear = settings.wear_cost.0 * (m.dc * hours).abs() / 1000.0;
    if forecast.islanded {
        // No grid: surplus PV is curtailed, and imports are only a penalty.
        return wear + excess_penalty(forecast, grid, settings, hours);
    }
    let import_kwh = grid.max(0.0) * hours / 1000.0;
    let export_kwh = (-grid).max(0.0) * hours / 1000.0;
    forecast.prices.buy.0 * import_kwh - forecast.prices.sell.0 * export_kwh
        + wear
        + excess_penalty(forecast, grid, settings, hours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn battery() -> BatteryModel {
        BatteryModel::multiplus_ii_prior(WattHours(10_000.0), 3, Watts(10_000.0), Watts(11_000.0))
    }

    fn settings() -> PlannerSettings {
        PlannerSettings {
            energy_step: WattHours(250.0),
            ..PlannerSettings::default()
        }
    }

    fn start() -> Timestamp {
        "2026-09-27T12:00:00Z".parse().unwrap()
    }

    /// Full slots from `start()` with the given buy prices; sell = buy (net metering).
    fn forecasts(prices: &[f64], load: f64, pv: f64) -> Vec<SlotForecast> {
        let mut slot = Slot::containing(start());
        prices
            .iter()
            .map(|&p| {
                let f = SlotForecast {
                    slot,
                    load: Watts(load),
                    pv: Watts(pv),
                    prices: SlotPrices {
                        buy: EurPerKwh(p),
                        sell: EurPerKwh(p),
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

    fn run(slots: &[SlotForecast], soc: f64, settings: &PlannerSettings) -> Plan {
        plan(&PlanRequest {
            now: start(),
            soc_pct: soc,
            pv_on: true,
            battery: &battery(),
            slots,
            settings,
        })
    }

    #[test]
    fn buys_cheap_and_uses_it_when_expensive() {
        let slots = forecasts(&[0.10, 0.10, 0.50, 0.50], 2000.0, 0.0);
        let p = run(&slots, 0.0, &settings());
        assert!(p.slots[0].battery_ac.0 > 1000.0 && p.slots[1].battery_ac.0 > 1000.0);
        assert!(p.slots[2].battery_ac.0 < 0.0 && p.slots[3].battery_ac.0 < 0.0);
        let without_battery: f64 = slots
            .iter()
            .map(|s| s.prices.buy.0 * (2000.0 + 60.0) * 0.25 / 1000.0)
            .sum();
        assert!(p.expected_cost < without_battery - 0.3);
    }

    #[test]
    fn small_spreads_are_not_worth_the_losses() {
        let slots = forecasts(&[0.30, 0.30, 0.31, 0.31], 2000.0, 0.0);
        let p = run(&slots, 50.0, &settings());
        assert!(
            p.slots[..2].iter().all(|s| s.battery_ac.0 <= 0.0),
            "no grid charging for a 1 ct spread"
        );
    }

    #[test]
    fn switches_pv_off_when_export_costs_money() {
        let mut slots = forecasts(&[0.20, 0.20, 0.20], 500.0, 6000.0);
        slots[1].prices.sell = EurPerKwh(-0.30);
        slots[1].prices.buy = EurPerKwh(-0.05);
        let mut s = settings();
        s.pv_switchable = true;
        let p = run(&slots, 100.0, &s);
        assert!(p.slots[0].pv_on && !p.slots[1].pv_on && p.slots[2].pv_on);
        assert!(
            p.slots[1].grid.0 > 0.0,
            "imports at a negative price instead"
        );
    }

    #[test]
    fn keeps_the_minimum_soc() {
        let mut slots = forecasts(&[0.60; 8], 3000.0, 0.0);
        for s in &mut slots {
            s.min_soc_end = 40.0;
        }
        let p = run(&slots, 60.0, &settings());
        assert!(p.slots.iter().all(|s| s.soc_end >= 40.0 - 1e-9));
        assert!(
            p.slots.last().unwrap().soc_end < 45.0,
            "uses everything above the floor"
        );
    }

    #[test]
    fn prepares_for_an_outage() {
        // Cheap now, an outage later: the battery must carry 2 kW for an hour.
        let mut slots = forecasts(
            &[0.10, 0.10, 0.10, 0.10, 0.30, 0.30, 0.30, 0.30],
            2000.0,
            0.0,
        );
        for s in &mut slots[4..] {
            s.islanded = true;
        }
        let p = run(&slots, 5.0, &settings());
        assert!(
            p.slots[..4].iter().any(|s| s.battery_ac.0 > 0.0),
            "charges before the outage"
        );
        for s in &p.slots[4..] {
            assert!(s.grid.0 <= 1e-6, "no imports during the outage: {s:?}");
            assert!(s.pv_on);
        }
    }

    #[test]
    fn covers_loads_beyond_the_grid_limit() {
        let slots = forecasts(&[0.30, 0.30], 20_000.0, 0.0);
        let p = run(&slots, 100.0, &settings());
        assert!(p.slots.iter().all(|s| s.grid.0 <= 17_000.0 + 1e-6));
    }

    #[test]
    fn the_first_slot_is_partial() {
        let slots = forecasts(&[0.10, 0.50], 1000.0, 0.0);
        let p = plan(&PlanRequest {
            now: start() + jiff::SignedDuration::from_mins(10),
            soc_pct: 50.0,
            pv_on: true,
            battery: &battery(),
            slots: &slots,
            settings: &settings(),
        });
        assert!((p.slots[0].hours - 5.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn stored_energy_is_worth_the_price_it_displaces() {
        // With expensive slots ahead, the last kWh stored is worth about the
        // buy price it avoids, minus discharge losses.
        let slots = forecasts(&[0.20, 0.40, 0.40, 0.40], 3000.0, 0.0);
        let p = run(&slots, 20.0, &settings());
        let value = p.slots[0].stored_energy_value.0;
        assert!(value > 0.30 && value < 0.40, "{value}");
    }

    proptest! {
        /// The optimum is never worse than leaving the battery idle.
        #[test]
        fn never_worse_than_an_idle_battery(
            prices in prop::collection::vec(-0.1f64..0.6, 1..12),
            load in 0.0f64..6000.0,
            pv in 0.0f64..8000.0,
            soc in 0.0f64..100.0,
        ) {
            let slots = forecasts(&prices, load, pv);
            let s = settings();
            let p = run(&slots, soc, &s);
            let b = battery();
            let level = ((soc / 100.0 * b.capacity.0) / s.energy_step.0).round();
            let idle = Move { levels: 0, ac: 0.0, dc: 0.0 };
            let idle_cost: f64 = slots.iter().map(|f| stage_cost(f, &b, &s, 0.25, &idle, true)).sum::<f64>()
                - s.terminal_value.0 * level * s.energy_step.0 / 1000.0;
            prop_assert!(p.expected_cost <= idle_cost + 1e-9);
            for slot in &p.slots {
                prop_assert!((0.0..=100.0 + 1e-9).contains(&slot.soc_end));
            }
        }
    }
}

#[cfg(test)]
mod timing {
    use super::*;

    /// `cargo test --release -p dess-core -- --ignored --nocapture realistic`
    #[test]
    #[ignore = "timing, run in release"]
    fn realistic_horizon() {
        let battery = BatteryModel::multiplus_ii_prior(
            WattHours(32_000.0),
            3,
            Watts(10_000.0),
            Watts(11_000.0),
        );
        let settings = PlannerSettings {
            pv_switchable: true,
            ..PlannerSettings::default()
        };
        let start: Timestamp = "2026-09-27T12:00:00Z".parse().unwrap();
        let mut slot = Slot::containing(start);
        let slots: Vec<_> = (0..192)
            .map(|t| {
                let price = 0.25 + 0.15 * (f64::from(t) * std::f64::consts::TAU / 96.0).sin();
                let f = SlotForecast {
                    slot,
                    load: Watts(800.0),
                    pv: Watts(
                        (4000.0 * (f64::from(t % 96) / 96.0 * std::f64::consts::PI).sin()).max(0.0),
                    ),
                    prices: SlotPrices {
                        buy: EurPerKwh(price),
                        sell: EurPerKwh(price - 0.02),
                    },
                    min_soc_end: 5.0,
                    estimated_price: t > 60,
                    islanded: false,
                };
                slot = slot.next();
                f
            })
            .collect();
        let started = std::time::Instant::now();
        let p = plan(&PlanRequest {
            now: start,
            soc_pct: 30.0,
            pv_on: true,
            battery: &battery,
            slots: &slots,
            settings: &settings,
        });
        println!(
            "192 slots × 321 levels: {:?}, expected cost €{:.2}",
            started.elapsed(),
            p.expected_cost
        );
    }
}
