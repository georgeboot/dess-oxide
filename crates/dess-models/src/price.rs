//! Day-ahead prices beyond the published ones, from what drives them: wind
//! and sun (in NL and Germany, which share the market), temperature, the
//! time of day and week, holidays, and the recent price level (gas and CO₂).
//!
//! Gradient-boosted trees ([`crate::gbdt`]), as EpexPredictor does, trained
//! on the last half year and retrained nightly. Like the other models it's
//! only used once it beats the fallback (the recent median of the same hour)
//! on held-out days.

use std::collections::BTreeMap;

use dess_core::calendar;
use dess_core::solar::sun_position;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::features::is_validation_hour;
use crate::gbdt::{self, Gbdt, Params};

/// Where the sun's position is taken: the middle of the Netherlands.
const CENTRE: (f64, f64) = (52.2, 5.3);
/// The recent price level: the mean over this many days before the day.
pub const LEVEL_DAYS: i64 = 14;

/// One hour: what's known of it in advance, and (for training) its price.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceHour {
    /// Unix seconds, the hour's start.
    pub hour: i64,
    /// Weather and renewables forecasts, in a fixed order.
    pub inputs: Vec<f64>,
    /// The mean price of the `LEVEL_DAYS` before the hour's day, €/MWh.
    pub level: f64,
    /// €/MWh; for training.
    pub price: f64,
}

/// The model's features for an hour: the inputs, then the calendar.
pub fn features(hour: i64, inputs: &[f64], level: f64, tz: &TimeZone) -> Vec<f64> {
    let at = Timestamp::from_second(hour + 1800).unwrap_or(Timestamp::UNIX_EPOCH);
    let local = at.to_zoned(tz.clone());
    let date = local.date();
    let weekday = f64::from(local.weekday().to_monday_zero_offset());
    let free = calendar::is_holiday(date) || weekday >= 6.0;
    let sun = sun_position(at, CENTRE.0, CENTRE.1);
    let mut x = inputs.to_vec();
    x.extend([
        f64::from(local.hour()),
        weekday,
        f64::from(u8::from(free)),
        sun.elevation_deg(),
        sun.azimuth.to_degrees(),
        f64::from(date.day_of_year()),
        level,
    ]);
    x
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceModel {
    pub trees: Gbdt,
    /// How many inputs it was trained with (its features depend on it).
    pub inputs: usize,
}

impl PriceModel {
    /// The expected price of an hour, €/MWh, or `None` if the inputs don't
    /// match what it was trained on.
    pub fn predict(&self, hour: i64, inputs: &[f64], level: f64, tz: &TimeZone) -> Option<f64> {
        (inputs.len() == self.inputs)
            .then(|| self.trees.predict(&features(hour, inputs, level, tz)))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceFit {
    pub model: PriceModel,
    pub hours: usize,
    /// Mean absolute error on held-out days, €/MWh.
    pub validation_mae: f64,
    /// The same for the fallback: the recent median of the same hour.
    pub baseline_mae: f64,
}

impl PriceFit {
    pub fn improves(&self) -> bool {
        self.validation_mae < self.baseline_mae
    }
}

/// Fits the trees on hours (oldest first); every fifth day is held out.
/// `None` without enough hours, or when they don't agree on their inputs.
pub fn fit(hours: &[PriceHour], tz: &TimeZone) -> Option<PriceFit> {
    let inputs = hours.first()?.inputs.len();
    if hours.len() < 24 * 30 || hours.iter().any(|h| h.inputs.len() != inputs) {
        return None;
    }
    let (validation, training): (Vec<&PriceHour>, Vec<&PriceHour>) =
        hours.iter().partition(|h| is_validation_hour(h.hour));
    let rows: Vec<Vec<f64>> = training
        .iter()
        .map(|h| features(h.hour, &h.inputs, h.level, tz))
        .collect();
    let targets: Vec<f64> = training.iter().map(|h| h.price).collect();
    let model = PriceModel {
        trees: gbdt::fit(&rows, &targets, &Params::default()),
        inputs,
    };
    let validation_mae =
        mean(validation.iter().filter_map(|h| {
            Some((model.predict(h.hour, &h.inputs, h.level, tz)? - h.price).abs())
        }));
    let prices: BTreeMap<i64, f64> = hours.iter().map(|h| (h.hour, h.price)).collect();
    let baseline_mae = mean(
        validation
            .iter()
            .filter_map(|h| Some((recent_median(&prices, h.hour)? - h.price).abs())),
    );
    Some(PriceFit {
        model,
        hours: hours.len(),
        validation_mae,
        baseline_mae,
    })
}

/// The fallback the planner used before: half the median of the same hour
/// over the last two weeks, half the median of all their hours (as in
/// `dess_core::prices`), from the day before on.
fn recent_median(prices: &BTreeMap<i64, f64>, hour: i64) -> Option<f64> {
    let day_start = hour - hour.rem_euclid(86_400);
    let window = prices.range(day_start - 14 * 86_400..day_start);
    let all: Vec<f64> = window.map(|(_, p)| *p).collect();
    let same: Vec<f64> = (1..=14)
        .filter_map(|d| prices.get(&(hour - d * 86_400)).copied())
        .collect();
    Some(0.5 * median(same)? + 0.5 * median(all)?)
}

fn median(mut values: Vec<f64>) -> Option<f64> {
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

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { f64::NAN } else { sum / n as f64 }
}

/// The mean price of the `LEVEL_DAYS` days before `hour`'s (UTC) day.
pub fn level(prices: &BTreeMap<i64, f64>, hour: i64) -> Option<f64> {
    let day_start = hour - hour.rem_euclid(86_400);
    let values: Vec<f64> = prices
        .range(day_start - LEVEL_DAYS * 86_400..day_start)
        .map(|(_, p)| *p)
        .collect();
    (values.len() >= 24 * 3).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A market where price follows wind and sun: windy or sunny hours are
    /// cheap, calm dark evenings dear.
    fn market() -> Vec<PriceHour> {
        let start = 1_767_225_600; // 2026-01-01
        let tz = TimeZone::UTC;
        (0..24 * 120)
            .map(|i: i64| {
                let hour = start + i * 3600;
                let wind = 6.0 + 5.0 * ((i as f64) * 0.05).sin() + 2.0 * ((i as f64) * 0.31).cos();
                let local = (i % 24) as f64;
                let sun = if (8.0..17.0).contains(&local) {
                    400.0
                } else {
                    0.0
                };
                let evening = if (17.0..21.0).contains(&local) {
                    40.0
                } else {
                    0.0
                };
                let price = 120.0 - 8.0 * wind - 0.1 * sun + evening;
                let _ = &tz;
                PriceHour {
                    hour,
                    inputs: vec![wind, sun],
                    level: 90.0,
                    price,
                }
            })
            .collect()
    }

    #[test]
    fn beats_the_recent_median_when_weather_drives_prices() {
        let fit = fit(&market(), &TimeZone::UTC).unwrap();
        assert!(
            fit.improves(),
            "{} vs {}",
            fit.validation_mae,
            fit.baseline_mae
        );
        assert!(fit.validation_mae < 5.0, "{}", fit.validation_mae);
    }

    #[test]
    fn inputs_must_match() {
        let fit = fit(&market(), &TimeZone::UTC).unwrap();
        assert!(fit.model.predict(0, &[1.0], 90.0, &TimeZone::UTC).is_none());
        assert!(
            fit.model
                .predict(0, &[5.0, 0.0], 90.0, &TimeZone::UTC)
                .is_some()
        );
    }
}
