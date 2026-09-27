//! Steady-state samples of the inverter/charger's AC↔DC conversion.
//!
//! Victron publishes no efficiency curves, so dess-oxide learns them. The
//! learner (M2) fits `loss(P) = a + b·|P| + c·P²` per direction; this module
//! only collects clean data for it. A sample counts when both the AC and the
//! DC side have been steady for a while, which removes the timing skew
//! between the inverter's and the BMS's measurements during transitions.

use std::collections::{BTreeMap, VecDeque};

use jiff::{SignedDuration, Timestamp};

use crate::units::Watts;

/// Width of one power bin, in W of AC conversion power.
pub const BIN_WIDTH_W: f64 = 100.0;

/// Sums over the samples in one power bin; enough for weighted least squares
/// without keeping the samples themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BinStats {
    pub n: u64,
    pub sum_ac: f64,
    pub sum_dc: f64,
    pub sum_ac2: f64,
    pub sum_dc2: f64,
    pub sum_ac_dc: f64,
    pub sum_voltage: f64,
}

impl BinStats {
    fn add(&mut self, ac: f64, dc: f64, voltage: f64) {
        self.n += 1;
        self.sum_ac += ac;
        self.sum_dc += dc;
        self.sum_ac2 += ac * ac;
        self.sum_dc2 += dc * dc;
        self.sum_ac_dc += ac * dc;
        self.sum_voltage += voltage;
    }
}

#[derive(Debug, Clone, Copy)]
struct Point {
    at: Timestamp,
    ac: f64,
    dc: f64,
}

/// Collects steady-state (AC, DC) conversion samples into power bins.
#[derive(Debug)]
pub struct EfficiencySampler {
    window: SignedDuration,
    points: VecDeque<Point>,
    bins: BTreeMap<i32, BinStats>,
}

impl Default for EfficiencySampler {
    fn default() -> Self {
        Self::new(SignedDuration::from_secs(20))
    }
}

impl EfficiencySampler {
    /// `window` is how long both sides must have been steady.
    pub fn new(window: SignedDuration) -> Self {
        Self {
            window,
            points: VecDeque::new(),
            bins: BTreeMap::new(),
        }
    }

    /// `inverter_ac` is AC-in minus AC-out (positive = charging); `dc` is the
    /// DC power at the inverter, i.e. battery power minus DC-coupled PV.
    pub fn push(&mut self, at: Timestamp, inverter_ac: Watts, dc: Watts, battery_voltage: f64) {
        if self.points.back().is_some_and(|last| at <= last.at) {
            return;
        }
        self.points.push_back(Point {
            at,
            ac: inverter_ac.0,
            dc: dc.0,
        });
        while self
            .points
            .front()
            .is_some_and(|first| at.duration_since(first.at) > self.window)
        {
            self.points.pop_front();
        }

        let spans_window = self.points.front().is_some_and(|first| {
            at.duration_since(first.at) >= self.window - SignedDuration::from_secs(1)
        });
        if spans_window
            && steady(self.points.iter().map(|p| p.ac))
            && steady(self.points.iter().map(|p| p.dc))
        {
            let bin = (inverter_ac.0 / BIN_WIDTH_W).round() as i32;
            self.bins
                .entry(bin)
                .or_default()
                .add(inverter_ac.0, dc.0, battery_voltage);
        }
    }

    /// Returns and clears the bins collected so far.
    pub fn take_bins(&mut self) -> BTreeMap<i32, BinStats> {
        std::mem::take(&mut self.bins)
    }
}

/// Whether the spread of `values` is within max(50 W, 3 % of the mean magnitude).
fn steady(values: impl Iterator<Item = f64>) -> bool {
    let (mut min, mut max, mut sum, mut n) = (f64::INFINITY, f64::NEG_INFINITY, 0.0, 0.0);
    for v in values {
        min = min.min(v);
        max = max.max(v);
        sum += v;
        n += 1.0;
    }
    n > 0.0 && max - min <= f64::max(50.0, 0.03 * (sum / n).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + s).unwrap()
    }

    #[test]
    fn steady_power_is_binned_after_the_window() {
        let mut sampler = EfficiencySampler::default();
        for s in 0..30 {
            sampler.push(t(s), Watts(3020.0), Watts(2800.0), 52.0);
        }
        let bins = sampler.take_bins();
        let stats = bins[&30];
        assert_eq!(stats.n, 11, "samples from 19 s onwards count");
        assert!((stats.sum_dc / stats.n as f64 - 2800.0).abs() < 1e-9);
        assert!(sampler.take_bins().is_empty());
    }

    #[test]
    fn transitions_are_ignored() {
        let mut sampler = EfficiencySampler::default();
        for s in 0..30 {
            let ac = if s < 15 { 1000.0 } else { 3000.0 };
            sampler.push(t(s), Watts(ac), Watts(ac * 0.93), 52.0);
        }
        assert!(sampler.take_bins().is_empty());
    }

    #[test]
    fn small_noise_is_steady() {
        let mut sampler = EfficiencySampler::default();
        for s in 0..25 {
            let noise = if s % 2 == 0 { 20.0 } else { -20.0 };
            sampler.push(t(s), Watts(-2000.0 + noise), Watts(-2150.0 - noise), 51.0);
        }
        assert!(sampler.take_bins().contains_key(&-20));
    }
}
