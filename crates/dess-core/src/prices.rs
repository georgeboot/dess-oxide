//! The price horizon: published day-ahead prices, extended with estimates.
//!
//! Before 12:55 CET only today's prices are known, which would leave the
//! planner with a horizon of a few hours and no idea what stored energy is
//! worth afterwards. So the horizon is extended with estimates: the median of
//! the same quarter hour over the past days, blended with the overall median.
//! Estimated slots are marked; they shape decisions but are never shown as
//! real prices.

use std::collections::BTreeMap;

use jiff::SignedDuration;

use crate::slot::Slot;
use crate::units::EurPerKwh;

/// One slot's spot price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpotPrice {
    pub slot: Slot,
    pub spot: EurPerKwh,
    pub estimated: bool,
}

/// Spot prices for `[from, until)`: known where published, estimated after:
/// from `forecast` where it has the slot (the price model), else from up to
/// `lookback_days` of `known` history before the slot. Returns `None` if
/// nothing is known at all.
pub fn horizon(
    known: &BTreeMap<Slot, EurPerKwh>,
    forecast: &BTreeMap<Slot, EurPerKwh>,
    from: Slot,
    until: Slot,
    lookback_days: u32,
) -> Option<Vec<SpotPrice>> {
    if known.is_empty() {
        return None;
    }
    let mut prices = Vec::new();
    let mut slot = from;
    while slot < until {
        let price = match known.get(&slot) {
            Some(&spot) => SpotPrice {
                slot,
                spot,
                estimated: false,
            },
            None => SpotPrice {
                slot,
                spot: forecast
                    .get(&slot)
                    .copied()
                    .unwrap_or_else(|| estimate(known, slot, lookback_days)),
                estimated: true,
            },
        };
        prices.push(price);
        slot = slot.next();
    }
    Some(prices)
}

fn estimate(known: &BTreeMap<Slot, EurPerKwh>, slot: Slot, lookback_days: u32) -> EurPerKwh {
    let window_start =
        Slot::containing(slot.start() - SignedDuration::from_hours(24 * i64::from(lookback_days)));
    let history: Vec<f64> = known.range(window_start..slot).map(|(_, p)| p.0).collect();
    let same_time: Vec<f64> = (1..=lookback_days)
        .filter_map(|days| {
            let earlier =
                Slot::containing(slot.start() - SignedDuration::from_hours(24 * i64::from(days)));
            known.get(&earlier).map(|p| p.0)
        })
        .collect();
    let overall = median(history).unwrap_or_else(|| {
        // Nothing before the slot: fall back to everything known.
        median(known.values().map(|p| p.0).collect()).expect("known is not empty")
    });
    EurPerKwh(median(same_time).map_or(overall, |typical| 0.5 * typical + 0.5 * overall))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(s: &str) -> Slot {
        Slot::containing(s.parse().unwrap())
    }

    /// Two days of history where 18:00 costs 0.40 and everything else 0.10.
    fn history() -> BTreeMap<Slot, EurPerKwh> {
        let mut known = BTreeMap::new();
        let mut s = slot("2026-09-25T00:00:00Z");
        while s < slot("2026-09-27T00:00:00Z") {
            let evening = s.start().to_string().contains("T18:00");
            known.insert(s, EurPerKwh(if evening { 0.40 } else { 0.10 }));
            s = s.next();
        }
        known
    }

    #[test]
    fn known_prices_pass_through() {
        let known = history();
        let h = horizon(
            &known,
            &BTreeMap::new(),
            slot("2026-09-26T18:00:00Z"),
            slot("2026-09-26T18:30:00Z"),
            14,
        )
        .unwrap();
        assert_eq!(h.len(), 2);
        assert!(!h[0].estimated);
        assert_eq!(h[0].spot, EurPerKwh(0.40));
    }

    #[test]
    fn estimates_keep_the_daily_shape_but_flatten_it() {
        let known = history();
        let h = horizon(
            &known,
            &BTreeMap::new(),
            slot("2026-09-27T17:45:00Z"),
            slot("2026-09-27T18:15:00Z"),
            14,
        )
        .unwrap();
        assert!(h.iter().all(|p| p.estimated));
        assert!((h[0].spot.0 - 0.10).abs() < 1e-12);
        assert!(
            (h[1].spot.0 - 0.25).abs() < 1e-12,
            "halfway between 0.40 and the 0.10 median"
        );
    }

    #[test]
    fn the_price_model_fills_in_where_it_can() {
        let known = history();
        let from = slot("2026-09-27T17:45:00Z");
        let forecast = BTreeMap::from([(from, EurPerKwh(0.33))]);
        let h = horizon(&known, &forecast, from, slot("2026-09-27T18:15:00Z"), 14).unwrap();
        assert_eq!(h[0].spot, EurPerKwh(0.33));
        assert!(h[0].estimated);
        assert!(
            (h[1].spot.0 - 0.25).abs() < 1e-12,
            "no forecast: the median"
        );
    }

    #[test]
    fn nothing_known_means_no_horizon() {
        assert!(
            horizon(
                &BTreeMap::new(),
                &BTreeMap::new(),
                slot("2026-09-27T00:00:00Z"),
                slot("2026-09-27T01:00:00Z"),
                14
            )
            .is_none()
        );
    }
}
