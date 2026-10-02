//! Steady-state samples of the inverter/charger's AC↔DC conversion.
//!
//! Victron publishes no efficiency curves, so dess-oxide learns them. The
//! learner (M2) fits `loss(P) = a + b·|P| + c·P²` per direction; this module
//! only collects clean data for it. A sample counts when both the AC and the
//! DC side have been steady for a while, which removes the timing skew
//! between the inverter's and the BMS's measurements during transitions.

use std::collections::{BTreeMap, VecDeque};

use jiff::{SignedDuration, Timestamp};

use crate::battery::LossCurve;
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

/// Losses learned from the bins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LearnedLosses {
    /// Constant draw of the inverter/chargers, W.
    pub standby: Option<f64>,
    pub charge: Option<LossCurve>,
    pub discharge: Option<LossCurve>,
}

/// Samples needed per direction, and the power range they must span, before
/// a direction's curve is trusted over the prior.
const MIN_SAMPLES: u64 = 600;
const MIN_SPAN_W: f64 = 1500.0;
/// Idle samples (ESS regulating, the inverters converting next to nothing)
/// needed before the measured standby draw replaces the prior.
const MIN_IDLE_SAMPLES: f64 = 300.0;
/// Older bins count less: half weight per month.
const HALF_LIFE_DAYS: f64 = 30.0;

/// Fits `loss(P) = a + b·P + c·P²` (with `loss = AC − DC` and `P = |AC|`)
/// for each direction by weighted least squares, with `b, c ≥ 0`. `bins` are
/// `(age in days, bin, stats)`.
///
/// The standby draw `a` is measured on its own, from the idle samples, and
/// the curves are fitted around it. Fitted together with the curves it's
/// poorly pinned down when idle samples are scarce (a controller that
/// bypasses the inverters whenever the battery idles leaves few), and a
/// wrong `a` bends the curves the other way. Until there are enough idle
/// samples, `standby_prior` stands in.
pub fn fit_losses(bins: &[(f64, i32, BinStats)], standby_prior: f64) -> LearnedLosses {
    let points: Vec<(f64, f64, f64)> = bins
        .iter()
        .filter(|(_, _, s)| s.n > 0)
        .map(|(age, _, s)| {
            let n = s.n as f64;
            let ac = s.sum_ac / n;
            let loss = (s.sum_ac - s.sum_dc) / n;
            (ac, loss, n * 0.5f64.powf(age / HALF_LIFE_DAYS))
        })
        .collect();
    let (idle_loss, idle_weight) = points
        .iter()
        .filter(|p| p.0.abs() <= 50.0)
        .fold((0.0, 0.0), |(l, w), p| (l + p.1 * p.2, w + p.2));
    let standby = (idle_weight >= MIN_IDLE_SAMPLES).then(|| (idle_loss / idle_weight).max(0.0));
    let a = standby.unwrap_or(standby_prior);
    let direction = |charging: bool| -> Option<LossCurve> {
        let active: Vec<(f64, f64, f64)> = points
            .iter()
            .filter(|p| p.0.abs() > 50.0 && (p.0 > 0.0) == charging)
            .map(|&(ac, loss, w)| (ac.abs(), loss - a, w))
            .collect();
        let samples: f64 = active.iter().map(|p| p.2).sum();
        let span = active.iter().map(|p| p.0).fold(0.0, f64::max)
            - active.iter().map(|p| p.0).fold(f64::MAX, f64::min);
        if samples < MIN_SAMPLES as f64 * 0.5 || span < MIN_SPAN_W {
            return None;
        }
        let [_, b, c] = constrained_through_origin(&active)?;
        Some(LossCurve {
            linear: b,
            quadratic: c,
        })
    };
    LearnedLosses {
        standby,
        charge: direction(true),
        discharge: direction(false),
    }
}

/// Weighted least squares for `y = b·x + c·x²` with `b, c ≥ 0`. When the
/// free fit has a negative term, the best allowed fit has that term at zero:
/// of the fits with one term dropped, the one that fits best. (Taking the
/// first allowed one instead turns a curve whose free fit has a slightly
/// negative linear term into a straight line.)
fn constrained_through_origin(points: &[(f64, f64, f64)]) -> Option<[f64; 3]> {
    let residual = |s: &[f64; 3]| -> f64 {
        points
            .iter()
            .map(|&(x, y, w)| w * (y - s[1] * x - s[2] * x * x).powi(2))
            .sum()
    };
    [
        [false, true, true],
        [false, true, false],
        [false, false, true],
    ]
    .into_iter()
    .filter_map(|terms| weighted_least_squares(points, terms))
    .filter(|s| s[1] >= 0.0 && s[2] >= 0.0)
    .min_by(|a, b| residual(a).total_cmp(&residual(b)))
}

/// Weighted least squares on the chosen terms of `[1, x, x²]`.
fn weighted_least_squares(points: &[(f64, f64, f64)], terms: [bool; 3]) -> Option<[f64; 3]> {
    let used: Vec<usize> = (0..3).filter(|&i| terms[i]).collect();
    let k = used.len();
    let mut ata = vec![vec![0.0; k]; k];
    let mut aty = vec![0.0; k];
    for &(x, y, w) in points {
        let basis = [1.0, x, x * x];
        for (i, &bi) in used.iter().enumerate() {
            aty[i] += w * basis[bi] * y;
            for (j, &bj) in used.iter().enumerate() {
                ata[i][j] += w * basis[bi] * basis[bj];
            }
        }
    }
    let solved = solve(ata, aty)?;
    let mut out = [0.0; 3];
    for (i, &term) in used.iter().enumerate() {
        out[term] = solved[i];
    }
    Some(out)
}

/// Gaussian elimination with partial pivoting.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        let pivot_row = a[col].clone();
        for row in col + 1..n {
            let factor = a[row][col] / pivot_row[col];
            for (x, p) in a[row][col..].iter_mut().zip(&pivot_row[col..]) {
                *x -= factor * p;
            }
            b[row] -= factor * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let sum: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - sum) / a[row][row];
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bins from a known curve: 40 W standby, charge 2 %·P + 1e-5·P².
    fn bins() -> Vec<(f64, i32, BinStats)> {
        (-80..=80)
            .filter(|b: &i32| b.unsigned_abs().is_multiple_of(5))
            .map(|b| {
                let ac = f64::from(b) * 100.0;
                let p = ac.abs();
                let (lin, quad) = if ac >= 0.0 {
                    (0.02, 1e-5)
                } else {
                    (0.01, 1.5e-5)
                };
                let loss = 40.0 + lin * p + quad * p * p;
                let dc = ac - loss;
                // Minutes of idling, under a minute at each power.
                let n = if b == 0 { 400u64 } else { 50 };
                let stats = BinStats {
                    n,
                    sum_ac: ac * n as f64,
                    sum_dc: dc * n as f64,
                    ..BinStats::default()
                };
                (1.0, b, stats)
            })
            .collect()
    }

    #[test]
    fn recovers_a_known_loss_curve() {
        let learned = fit_losses(&bins(), 60.0);
        assert!((learned.standby.unwrap() - 40.0).abs() < 1e-6);
        let charge = learned.charge.unwrap();
        assert!((charge.linear - 0.02).abs() < 1e-6 && (charge.quadratic - 1e-5).abs() < 1e-9);
        let discharge = learned.discharge.unwrap();
        assert!(
            (discharge.linear - 0.01).abs() < 1e-6 && (discharge.quadratic - 1.5e-5).abs() < 1e-9
        );
    }

    #[test]
    fn too_little_data_keeps_the_prior() {
        let few: Vec<_> = bins()
            .into_iter()
            .filter(|(_, b, _)| (0..=10).contains(b))
            .collect();
        let learned = fit_losses(&few, 60.0);
        assert!(learned.charge.is_none() && learned.discharge.is_none());
    }

    #[test]
    fn a_curve_is_not_flattened_into_a_line() {
        // Charging losses that grow with the square of the power, measured
        // with a standby a little under the prior: the free fit's linear term
        // comes out slightly negative. The best allowed fit is the curve.
        let curve: Vec<_> = (15..=120)
            .step_by(5)
            .map(|b: i32| {
                let ac = f64::from(b) * 100.0;
                let loss = 50.0 + 9e-6 * ac * ac;
                let stats = BinStats {
                    n: 50,
                    sum_ac: ac * 50.0,
                    sum_dc: (ac - loss) * 50.0,
                    ..BinStats::default()
                };
                (1.0, b, stats)
            })
            .collect();
        let charge = fit_losses(&curve, 60.0).charge.unwrap();
        assert!(charge.quadratic > 5e-6, "{charge:?}");
        let efficiency = |p: f64| 1.0 - charge.linear - charge.quadratic * p;
        assert!(
            efficiency(3000.0) - efficiency(10_000.0) > 0.04,
            "{charge:?}"
        );
    }

    #[test]
    fn without_idle_samples_the_standby_prior_anchors_the_curves() {
        // As under a controller that bypasses whenever the battery idles:
        // no samples near zero power.
        let busy: Vec<_> = bins()
            .into_iter()
            .filter(|(_, b, _)| b.abs() >= 5)
            .collect();
        let learned = fit_losses(&busy, 40.0);
        assert_eq!(learned.standby, None, "not measured");
        let discharge = learned.discharge.unwrap();
        assert!(
            (discharge.linear - 0.01).abs() < 1e-6 && (discharge.quadratic - 1.5e-5).abs() < 1e-9,
            "{discharge:?}"
        );
    }

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
