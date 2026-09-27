//! Usable battery capacity, learned from long one-way SoC stretches
//! (PLAN.md §12.2): `C = ∫P_dc dt / ΔSoC`.
//!
//! It's the capacity on the BMS's own SoC scale, which is what the planner
//! needs to turn SoC into energy. Charge stretches come out larger than
//! discharge ones by the battery's own losses; their ratio is its DC
//! round-trip efficiency.

/// DC energy and SoC for one recorded 15-minute slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotEnergy {
    /// Unix seconds; consecutive slots are 900 apart.
    pub start: i64,
    pub soc_start: f64,
    pub soc_end: f64,
    pub charge_wh: f64,
    pub discharge_wh: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacityFit {
    /// Median over charge stretches, Wh per 100 % of SoC.
    pub charge_wh: Option<f64>,
    /// Median over discharge stretches.
    pub discharge_wh: Option<f64>,
    pub stretches: usize,
}

impl CapacityFit {
    /// The capacity to plan with: halfway between the two directions (the
    /// planner applies losses to both).
    pub fn usable_wh(&self) -> Option<f64> {
        match (self.charge_wh, self.discharge_wh) {
            (Some(c), Some(d)) => Some((c * d).sqrt()),
            (one, other) => one.or(other).filter(|_| self.stretches >= 2),
        }
    }

    /// The battery's own DC round-trip efficiency.
    pub fn round_trip(&self) -> Option<f64> {
        Some(self.discharge_wh? / self.charge_wh?)
    }
}

/// A stretch must move the SoC at least this much.
const MIN_SOC_SPAN: f64 = 30.0;
/// Slots moving less DC energy than this don't end a stretch.
const IDLE_WH: f64 = 25.0;
/// Near full the BMS may resync its SoC to 100 %; charging stops counting
/// there.
const NEAR_FULL: f64 = 99.0;

/// Fits the capacity from recorded slots in time order.
pub fn fit_capacity(slots: &[SlotEnergy]) -> CapacityFit {
    let mut charge = Vec::new();
    let mut discharge = Vec::new();
    let mut close = |run: &[SlotEnergy]| {
        let (Some(first), Some(last)) = (run.first(), run.last()) else {
            return;
        };
        let span = last.soc_end - first.soc_start;
        let energy: f64 = run.iter().map(|s| s.charge_wh - s.discharge_wh).sum();
        if span.abs() >= MIN_SOC_SPAN && energy * span > 0.0 {
            let capacity = energy / span * 100.0;
            if span > 0.0 {
                charge.push(capacity);
            } else {
                discharge.push(capacity);
            }
        }
    };

    let mut run: Vec<SlotEnergy> = Vec::new();
    let mut direction = 0.0_f64;
    for &slot in slots {
        let net = slot.charge_wh - slot.discharge_wh;
        let this = if net.abs() <= IDLE_WH {
            0.0
        } else {
            net.signum()
        };
        let contiguous = run
            .last()
            .is_some_and(|last| slot.start == last.start + 900);
        let reverses = this != 0.0 && direction != 0.0 && this != direction;
        let full = this > 0.0 && slot.soc_end >= NEAR_FULL;
        if !contiguous || reverses || full {
            close(&run);
            run.clear();
            direction = 0.0;
        }
        if full {
            continue;
        }
        if direction == 0.0 {
            direction = this;
        }
        run.push(slot);
    }
    close(&run);

    let stretches = charge.len() + discharge.len();
    CapacityFit {
        charge_wh: median(&mut charge),
        discharge_wh: median(&mut discharge),
        stretches,
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 30 kWh battery (on the BMS's scale) moving `wh` per slot from `soc`.
    fn stretch(start: i64, soc: f64, wh: f64, slots: u8, losses: f64) -> Vec<SlotEnergy> {
        let mut soc = soc;
        (0..i64::from(slots))
            .map(|i| {
                let stored = if wh > 0.0 { wh / losses } else { wh * losses };
                let next = soc + stored / 30_000.0 * 100.0;
                let slot = SlotEnergy {
                    start: start + 900 * i,
                    soc_start: soc,
                    soc_end: next,
                    charge_wh: wh.max(0.0),
                    discharge_wh: (-wh).max(0.0),
                };
                soc = next;
                slot
            })
            .collect()
    }

    #[test]
    fn learns_capacity_and_round_trip() {
        // Charge 20 → 80 %, then discharge 80 → 30 %, with 2 % battery
        // losses each way.
        let mut slots = stretch(0, 20.0, 1000.0, 18, 1.02);
        let end = slots.last().unwrap().soc_end;
        slots.extend(stretch(18 * 900, end, -1000.0, 15, 1.02));
        let fit = fit_capacity(&slots);
        assert_eq!(fit.stretches, 2);
        let (c, d) = (fit.charge_wh.unwrap(), fit.discharge_wh.unwrap());
        assert!((c - 30_600.0).abs() < 1.0, "{c}");
        assert!((d - 30_000.0 / 1.02).abs() < 1.0, "{d}");
        assert!((fit.usable_wh().unwrap() - 30_000.0).abs() < 1.0);
        assert!((fit.round_trip().unwrap() - 1.0 / 1.02_f64.powi(2)).abs() < 1e-6);
    }

    #[test]
    fn ignores_short_stretches_gaps_and_the_top() {
        // 20 % only.
        assert_eq!(fit_capacity(&stretch(0, 40.0, 1000.0, 6, 1.0)).stretches, 0);
        // A gap splits 40 % into two 20 % halves.
        let mut gap = stretch(0, 20.0, 1000.0, 6, 1.0);
        gap.extend(stretch(10 * 900, 40.0, 1000.0, 6, 1.0));
        assert_eq!(fit_capacity(&gap).stretches, 0);
        // Charging into 100 % ends where it gets near full.
        let top = stretch(0, 60.0, 1000.0, 14, 1.0);
        assert_eq!(fit_capacity(&top).stretches, 1);
        let fit = fit_capacity(&top);
        assert!((fit.charge_wh.unwrap() - 30_000.0).abs() < 1.0);
    }
}
