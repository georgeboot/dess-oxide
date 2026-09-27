//! Baseline forecasts for M1, until M2's learned models replace them.

use std::collections::HashMap;

use jiff::tz::TimeZone;

use crate::slot::Slot;
use crate::units::Watts;

/// Mean load per local quarter hour of the day over `history`.
///
/// Falls back to the overall mean for quarter hours without history, and to
/// `fallback` without any history at all.
pub fn baseline_load(
    history: &[(Slot, Watts)],
    targets: &[Slot],
    time_zone: &TimeZone,
    fallback: Watts,
) -> Vec<Watts> {
    let quarter_of_day = |slot: Slot| {
        let time = slot.start().to_zoned(time_zone.clone()).time();
        i32::from(time.hour()) * 4 + i32::from(time.minute()) / 15
    };
    let mut by_quarter: HashMap<i32, (f64, u32)> = HashMap::new();
    for &(slot, load) in history {
        let entry = by_quarter.entry(quarter_of_day(slot)).or_default();
        entry.0 += load.0;
        entry.1 += 1;
    }
    let overall = if history.is_empty() {
        fallback
    } else {
        Watts(history.iter().map(|(_, w)| w.0).sum::<f64>() / history.len() as f64)
    };
    targets
        .iter()
        .map(|&slot| {
            by_quarter
                .get(&quarter_of_day(slot))
                .map_or(overall, |&(sum, n)| Watts(sum / f64::from(n)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(s: &str) -> Slot {
        Slot::containing(s.parse().unwrap())
    }

    #[test]
    fn averages_the_same_quarter_hour_and_falls_back() {
        let tz = TimeZone::get("Europe/Amsterdam").unwrap();
        let history = [
            (slot("2026-09-25T16:00:00Z"), Watts(1000.0)),
            (slot("2026-09-26T16:00:00Z"), Watts(3000.0)),
            (slot("2026-09-26T03:00:00Z"), Watts(200.0)),
        ];
        let targets = [slot("2026-09-27T16:00:00Z"), slot("2026-09-27T10:00:00Z")];
        let forecast = baseline_load(&history, &targets, &tz, Watts(500.0));
        assert_eq!(forecast[0], Watts(2000.0));
        assert_eq!(
            forecast[1],
            Watts(1400.0),
            "overall mean where the quarter hour has no history"
        );
        assert_eq!(
            baseline_load(&[], &targets, &tz, Watts(500.0))[0],
            Watts(500.0)
        );
    }
}
