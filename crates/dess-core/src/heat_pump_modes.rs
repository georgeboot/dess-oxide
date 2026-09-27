//! Splits a heat pump's metered energy by what it was doing: heating or hot
//! water (and, within hot water, legionella runs), per 15-minute slot.
//!
//! The inputs are state histories: cumulative meter readings, and the times
//! the mode and the legionella flag changed. The energy between two readings
//! is spread evenly over the time between them, and each piece goes to the
//! mode and slot it falls in. Where the mode isn't known (before OpenAmber,
//! or while its state was unavailable) the energy still counts in the total,
//! but not as labelled.

use jiff::{SignedDuration, Timestamp};

use crate::slot::Slot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Heating,
    HotWater,
    /// Switching, starting up, maintenance: known, but neither.
    Other,
}

impl Mode {
    /// OpenAmber's "Control loop state MAIN"; `None` for unavailable/unknown.
    pub fn from_openamber(state: &str) -> Option<Self> {
        match state {
            "DHW" => Some(Self::HotWater),
            "Heat/Cool" => Some(Self::Heating),
            "unavailable" | "unknown" | "" => None,
            _ => Some(Self::Other),
        }
    }
}

/// Energy per slot, Wh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModeSlot {
    pub slot: Slot,
    /// Seconds with meter readings on both sides.
    pub covered_seconds: f64,
    /// Of those, seconds with a known mode.
    pub labelled_seconds: f64,
    pub total_wh: f64,
    pub hot_water_wh: f64,
    /// Part of `hot_water_wh`.
    pub legionella_wh: f64,
}

impl ModeSlot {
    fn new(slot: Slot) -> Self {
        Self {
            slot,
            covered_seconds: 0.0,
            labelled_seconds: 0.0,
            total_wh: 0.0,
            hot_water_wh: 0.0,
            legionella_wh: 0.0,
        }
    }

    /// Whether the whole slot was metered with a known mode.
    pub fn fully_labelled(&self) -> bool {
        self.labelled_seconds >= 890.0
    }
}

/// Readings further apart than this are a gap, not a slow meter.
const MAX_READING_GAP: SignedDuration = SignedDuration::from_mins(30);
/// More than this between two readings is a reset or a glitch.
const MAX_KWH_PER_HOUR: f64 = 15.0;

/// The value of a step function at `at`: the last change at or before it.
fn value_at<T: Copy>(changes: &[(Timestamp, T)], at: Timestamp) -> Option<T> {
    let i = changes.partition_point(|(t, _)| *t <= at);
    i.checked_sub(1).map(|i| changes[i].1)
}

/// Splits `energy` (cumulative kWh readings) over `[from, until)`. `mode`
/// and `legionella` are change histories (a `None` mode is unknown). All
/// three are in time order.
pub fn split(
    energy: &[(Timestamp, f64)],
    mode: &[(Timestamp, Option<Mode>)],
    legionella: &[(Timestamp, bool)],
    from: Timestamp,
    until: Timestamp,
) -> Vec<ModeSlot> {
    let mut slots: Vec<ModeSlot> = Vec::new();
    let mut cuts: Vec<Timestamp> = mode
        .iter()
        .map(|(t, _)| *t)
        .chain(legionella.iter().map(|(t, _)| *t))
        .collect();
    cuts.sort();

    for pair in energy.windows(2) {
        let ((ta, ea), (tb, eb)) = (pair[0], pair[1]);
        let interval = tb.duration_since(ta);
        let full_seconds = interval.as_secs_f64();
        if full_seconds <= 0.0 || interval > MAX_READING_GAP {
            continue;
        }
        let kwh = eb - ea;
        if !(0.0..=MAX_KWH_PER_HOUR * full_seconds / 3600.0 + 0.01).contains(&kwh) {
            continue;
        }
        // The rate over the whole interval; only the part in [from, until) counts.
        let wh_per_second = kwh * 1000.0 / full_seconds;
        let (a, b) = (ta.max(from), tb.min(until));
        if b <= a {
            continue;
        }
        // Cut [a, b) at mode and legionella changes and slot boundaries.
        let mut start = a;
        while start < b {
            let slot = Slot::containing(start);
            let next_cut = cuts
                .get(cuts.partition_point(|t| *t <= start))
                .copied()
                .unwrap_or(b);
            let end = b.min(slot.end()).min(next_cut);
            let piece = end.duration_since(start).as_secs_f64();
            let wh = wh_per_second * piece;
            if slots.last().is_none_or(|s| s.slot != slot) {
                slots.push(ModeSlot::new(slot));
            }
            let entry = slots.last_mut().expect("just pushed");
            entry.covered_seconds += piece;
            entry.total_wh += wh;
            if let Some(m) = value_at(mode, start).flatten() {
                entry.labelled_seconds += piece;
                if m == Mode::HotWater {
                    entry.hot_water_wh += wh;
                    if value_at(legionella, start).unwrap_or(false) {
                        entry.legionella_wh += wh;
                    }
                }
            }
            start = end;
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        "2026-09-27T10:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_mins(minutes)
    }

    #[test]
    fn splits_by_mode_and_slot() {
        // 2 kW for 30 minutes: hot water for the first 10, then heating.
        let energy: Vec<(Timestamp, f64)> = (0..=30)
            .map(|m| (at(m), 100.0 + 2.0 * m as f64 / 60.0))
            .collect();
        let mode = [
            (at(-5), Mode::from_openamber("DHW")),
            (at(10), Mode::from_openamber("Heat/Cool")),
        ];
        let slots = split(&energy, &mode, &[], at(0), at(60));
        assert_eq!(slots.len(), 2);
        let (first, second) = (slots[0], slots[1]);
        assert!((first.total_wh - 500.0).abs() < 1e-6, "{first:?}");
        assert!((first.hot_water_wh - 333.333).abs() < 0.01, "{first:?}");
        assert!(first.fully_labelled());
        assert!((second.total_wh - 500.0).abs() < 1e-6);
        assert_eq!(second.hot_water_wh, 0.0);
    }

    #[test]
    fn legionella_is_part_of_hot_water_and_unknown_mode_is_unlabelled() {
        let energy: Vec<(Timestamp, f64)> = (0..=15).map(|m| (at(m), m as f64 / 60.0)).collect();
        let mode = [
            (at(0), Mode::from_openamber("DHW")),
            (at(5), Mode::from_openamber("unavailable")),
        ];
        let legionella = [(at(0), true)];
        let slots = split(&energy, &mode, &legionella, at(0), at(15));
        let s = slots[0];
        assert!((s.total_wh - 250.0).abs() < 1e-6);
        assert!((s.hot_water_wh - 83.333).abs() < 0.01);
        assert!((s.legionella_wh - s.hot_water_wh).abs() < 1e-9);
        assert!((s.labelled_seconds - 300.0).abs() < 1e-6);
        assert!(!s.fully_labelled());
    }

    #[test]
    fn a_window_takes_only_its_share_of_an_interval() {
        // 1 kWh over 10:00–10:20; the window starts at 10:10.
        let energy = [(at(0), 0.0), (at(20), 1.0)];
        let slots = split(&energy, &[], &[], at(10), at(60));
        let total: f64 = slots.iter().map(|s| s.total_wh).sum();
        assert!((total - 500.0).abs() < 1e-6, "{slots:?}");
    }

    #[test]
    fn skips_gaps_and_resets() {
        let energy = [(at(0), 10.0), (at(45), 11.0), (at(46), 0.0), (at(47), 0.1)];
        let slots = split(&energy, &[], &[], at(0), at(60));
        // Only 46–47 counts: 45 minutes is a gap, and the meter went backwards.
        let total: f64 = slots.iter().map(|s| s.total_wh).sum();
        assert!((total - 100.0).abs() < 1e-6, "{slots:?}");
    }
}
