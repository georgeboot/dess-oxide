//! Hot water (DHW), learned from days where OpenAmber's mode says which
//! energy went into the tank.
//!
//! OpenAmber heats the tank only inside its schedule window, when the water
//! is below the setpoint minus a restart delta, and on legionella runs. So
//! per day:
//!
//! - **how much:** the day's hot water energy against its mean outdoor
//!   temperature (colder air and colder mains water both cost more), fitted
//!   as `a + b · max(0, 15 °C − T)`;
//! - **when:** the share of it per hour of the day over the recent days,
//!   which follows the schedule window;
//! - **legionella:** the extra energy of a run, placed at the next run's time
//!   (which OpenAmber announces).
//!
//! It's plain statistics: with a few weeks of days there's nothing for a
//! neural network to add.

use jiff::civil::Date;

/// Below this outdoor temperature, hot water costs more.
const REFERENCE_C: f64 = 15.0;
/// Days needed before the model is used.
pub const MIN_DAYS: usize = 7;
/// The timing comes from this many most recent days, so a changed schedule
/// shows up within a week or two.
const RECENT_DAYS: usize = 14;

/// One day with the mode known all day.
#[derive(Debug, Clone, PartialEq)]
pub struct HotWaterDay {
    pub date: Date,
    /// Mean outdoor temperature, °C.
    pub mean_temperature: f64,
    /// Hot water energy per local hour, without legionella runs, kWh.
    pub by_hour: [f64; 24],
    /// Legionella runs' energy that day, kWh.
    pub legionella_kwh: f64,
}

impl HotWaterDay {
    pub fn total_kwh(&self) -> f64 {
        self.by_hour.iter().sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotWaterModel {
    /// Daily energy at 15 °C and above, kWh.
    pub base_kwh: f64,
    /// Extra per degree below 15 °C, kWh.
    pub per_degree_kwh: f64,
    /// Share of the day's hot water per local hour, summing to 1.
    pub profile: [f64; 24],
    /// Energy of a legionella run, kWh (0 without runs seen).
    pub legionella_kwh: f64,
    pub days: usize,
    /// Mean absolute error of the daily energy, in-sample, kWh.
    pub daily_mae: f64,
}

impl HotWaterModel {
    pub fn daily_kwh(&self, mean_temperature: f64) -> f64 {
        self.base_kwh + self.per_degree_kwh * (REFERENCE_C - mean_temperature).max(0.0)
    }
}

/// Fits the model on days in date order; `None` with too few days.
pub fn fit(days: &[HotWaterDay]) -> Option<HotWaterModel> {
    if days.len() < MIN_DAYS {
        return None;
    }
    // Least squares on x = max(0, 15 − T); the slope only with enough spread.
    let xs: Vec<f64> = days
        .iter()
        .map(|d| (REFERENCE_C - d.mean_temperature).max(0.0))
        .collect();
    let ys: Vec<f64> = days.iter().map(HotWaterDay::total_kwh).collect();
    let n = days.len() as f64;
    let (mean_x, mean_y) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    let sxy: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let spread =
        xs.iter().copied().fold(f64::MIN, f64::max) - xs.iter().copied().fold(f64::MAX, f64::min);
    let per_degree = if spread >= 5.0 && sxx > 0.0 {
        (sxy / sxx).max(0.0)
    } else {
        0.0
    };
    let base = (mean_y - per_degree * mean_x).max(0.0);

    let recent = &days[days.len().saturating_sub(RECENT_DAYS)..];
    let mut profile = [0.0; 24];
    for day in recent {
        for (p, kwh) in profile.iter_mut().zip(day.by_hour) {
            *p += kwh;
        }
    }
    let sum: f64 = profile.iter().sum();
    if sum > 0.0 {
        for p in &mut profile {
            *p /= sum;
        }
    } else {
        profile = [1.0 / 24.0; 24];
    }

    let runs: Vec<f64> = days
        .iter()
        .map(|d| d.legionella_kwh)
        .filter(|&kwh| kwh > 0.05)
        .collect();
    let legionella = if runs.is_empty() {
        0.0
    } else {
        runs.iter().sum::<f64>() / runs.len() as f64
    };

    let model = HotWaterModel {
        base_kwh: base,
        per_degree_kwh: per_degree,
        profile,
        legionella_kwh: legionella,
        days: days.len(),
        daily_mae: 0.0,
    };
    let daily_mae = days
        .iter()
        .map(|d| (model.daily_kwh(d.mean_temperature) - d.total_kwh()).abs())
        .sum::<f64>()
        / n;
    Some(HotWaterModel { daily_mae, ..model })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(i: i64, temperature: f64, kwh: f64, legionella: f64) -> HotWaterDay {
        let mut by_hour = [0.0; 24];
        by_hour[11] = kwh * 0.75;
        by_hour[12] = kwh * 0.25;
        HotWaterDay {
            date: jiff::civil::date(2026, 1, 1)
                .checked_add(jiff::Span::new().days(i))
                .unwrap(),
            mean_temperature: temperature,
            by_hour,
            legionella_kwh: legionella,
        }
    }

    #[test]
    fn learns_amount_timing_and_legionella() {
        // 2 kWh a day at 15 °C, 0.1 kWh more per degree colder; a
        // legionella run of 1.2 kWh every seventh day.
        let days: Vec<HotWaterDay> = (0..28)
            .map(|i| {
                let t = 15.0 - (i % 10) as f64;
                let legionella = if i % 7 == 6 { 1.2 } else { 0.0 };
                day(i, t, 2.0 + 0.1 * (15.0 - t), legionella)
            })
            .collect();
        let m = fit(&days).unwrap();
        assert!((m.base_kwh - 2.0).abs() < 1e-6, "{m:?}");
        assert!((m.per_degree_kwh - 0.1).abs() < 1e-6);
        assert!((m.daily_kwh(5.0) - 3.0).abs() < 1e-6);
        assert!((m.profile[11] - 0.75).abs() < 1e-6 && (m.profile[12] - 0.25).abs() < 1e-6);
        assert!((m.legionella_kwh - 1.2).abs() < 1e-6);
        assert!(m.daily_mae < 1e-6);
    }

    #[test]
    fn needs_a_week_and_a_temperature_spread_for_the_slope() {
        let few: Vec<HotWaterDay> = (0..5).map(|i| day(i, 10.0, 2.0, 0.0)).collect();
        assert!(fit(&few).is_none());
        // Mild days only: no slope, just the mean.
        let mild: Vec<HotWaterDay> = (0..10)
            .map(|i| day(i, 14.0 + (i % 2) as f64, 2.0 + (i % 2) as f64 * 0.2, 0.0))
            .collect();
        let m = fit(&mild).unwrap();
        assert_eq!(m.per_degree_kwh, 0.0);
        assert!((m.base_kwh - 2.1).abs() < 1e-6);
    }
}
