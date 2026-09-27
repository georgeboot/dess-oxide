//! Usable battery capacity, learned from long one-way SoC stretches
//! (docs/DESIGN.md §7.2): `C = ∫P_dc dt / ΔSoC`.
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

/// Cycles of the battery a counter record needs before its round trip is
/// trusted: fewer, and the swing inside the record hides the drift.
const MIN_CYCLES: f64 = 20.0;

/// The cells' round trip from hourly energy counters at the battery's
/// terminals, `(kWh in, kWh out)` per hour in time order.
///
/// Dividing the energy out by the energy in is off by however much more or
/// less the battery holds at the end than at the start, up to its whole
/// capacity. But the stored energy has no trend: however long the record,
/// it stays between empty and full. So over the long run energy goes out at
/// the round trip's share of the rate it goes in, and that ratio of the two
/// counters' trends (least squares over every hour, not just the ends) is
/// the round trip. `None` until the battery has cycled enough for the swing
/// within the record not to matter.
pub fn round_trip_from_counters(hours: &[(f64, f64)]) -> Option<f64> {
    let n = hours.len() as f64;
    let (mut into, mut out) = (0.0, 0.0);
    let (mut st, mut stt, mut si, mut sti, mut so, mut sto) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for (t, &(i, o)) in hours.iter().enumerate() {
        let t = t as f64;
        into += i;
        out += o;
        st += t;
        stt += t * t;
        si += into;
        sti += t * into;
        so += out;
        sto += t * out;
    }
    let spread = stt - st * st / n;
    let trend_in = (sti - st * si / n) / spread;
    let trend_out = (sto - st * so / n) / spread;
    if trend_in.is_nan() || trend_in <= 0.0 {
        return None;
    }
    let round_trip = trend_out / trend_in;
    // How far the stored energy swings at that round trip: the record has to
    // hold many times that.
    let each_way = round_trip.sqrt();
    let (mut level, mut low, mut high) = (0.0_f64, 0.0_f64, 0.0_f64);
    for &(i, o) in hours {
        level += i * each_way - o / each_way;
        low = low.min(level);
        high = high.max(level);
    }
    ((0.8..1.0).contains(&round_trip) && into >= MIN_CYCLES * (high - low)).then_some(round_trip)
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

    /// A 30 kWh battery that loses 3 % each way (94.09 % round trip),
    /// cycling unevenly for `days`, starting and ending half full.
    fn counters(days: usize) -> Vec<(f64, f64)> {
        let each_way: f64 = 0.97;
        let mut stored = 15.0;
        let mut hours = Vec::new();
        for day in 0..days {
            // Charge towards a different top each day, then discharge.
            let top = 22.0 + 8.0 * ((day as f64) * 0.7).sin().abs();
            let bottom = 3.0 + 4.0 * ((day as f64) * 1.3).cos().abs();
            for _ in 0..12 {
                let into = ((top - stored) / each_way).clamp(0.0, 3.0);
                stored += into * each_way;
                hours.push((into, 0.0));
            }
            for _ in 0..12 {
                let out = ((stored - bottom) * each_way).clamp(0.0, 2.5);
                stored -= out / each_way;
                hours.push((0.0, out));
            }
        }
        hours
    }

    #[test]
    fn round_trip_from_the_counters_ignores_where_the_record_starts_and_ends() {
        let hours = counters(60);
        let rt = round_trip_from_counters(&hours).unwrap();
        assert!((rt - 0.9409).abs() < 0.002, "{rt}");
        // Start the record with the battery full: out over in is off by the
        // energy it held, the trends aren't.
        let cut = &hours[12..];
        let naive = cut.iter().map(|h| h.1).sum::<f64>() / cut.iter().map(|h| h.0).sum::<f64>();
        assert!((naive - 0.9409).abs() > 0.01, "{naive}");
        let rt = round_trip_from_counters(cut).unwrap();
        assert!((rt - 0.9409).abs() < 0.002, "{rt}");
        // A few days aren't enough.
        assert_eq!(round_trip_from_counters(&counters(5)), None);
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
