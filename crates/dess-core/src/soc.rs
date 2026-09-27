//! A finer state of charge than the BMS reports (PLAN.md §12.2).
//!
//! Some BMSes report whole percent: 320 Wh steps on a 32 kWh battery, coarser
//! than the planner's 100 Wh grid. Between reports we integrate battery DC
//! power, keep the estimate within the reported value's rounding band, and
//! re-anchor exactly where the reported value steps: that's where the true
//! SoC crossed the boundary between the two values. A BMS that reports
//! fractions passes straight through.

use jiff::{SignedDuration, Timestamp};

use crate::units::{WattHours, Watts};

/// After a longer gap, start over from the reported value.
const MAX_GAP: SignedDuration = SignedDuration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SocEstimator {
    capacity_wh: f64,
    state: Option<State>,
}

#[derive(Debug, Clone, Copy)]
struct State {
    at: Timestamp,
    pct: f64,
    reported: f64,
}

impl SocEstimator {
    pub fn new(capacity: WattHours) -> Self {
        Self {
            capacity_wh: capacity.0,
            state: None,
        }
    }

    pub fn set_capacity(&mut self, capacity: WattHours) {
        self.capacity_wh = capacity.0;
    }

    /// The estimate after a reading: the BMS's SoC (%) and battery DC power
    /// (positive = charging).
    pub fn update(&mut self, at: Timestamp, reported: f64, battery_dc: Watts) -> f64 {
        let whole = (reported - reported.round()).abs() < 1e-6;
        let pct = match self.state {
            Some(last)
                if whole
                    && self.capacity_wh > 0.0
                    && at > last.at
                    && at.duration_since(last.at) <= MAX_GAP
                    && (reported - last.reported).abs() <= 1.0 + 1e-6 =>
            {
                if (reported - last.reported).abs() > 1e-6 {
                    f64::midpoint(reported, last.reported)
                } else {
                    let hours = at.duration_since(last.at).as_secs_f64() / 3600.0;
                    let integrated = last.pct + battery_dc.0 * hours / self.capacity_wh * 100.0;
                    integrated.clamp(reported - 0.5, reported + 0.5)
                }
            }
            _ => reported,
        }
        .clamp(0.0, 100.0);
        self.state = Some(State { at, pct, reported });
        pct
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + seconds).unwrap()
    }

    #[test]
    fn follows_charging_between_whole_percent_steps() {
        // 32 kWh charged at 3.2 kW: 1 % per 6 minutes.
        let mut soc = SocEstimator::new(WattHours(32_000.0));
        let charge = Watts(3200.0);
        assert_eq!(soc.update(at(0), 50.0, charge), 50.0);
        // The step from 50 to 51 means the true SoC is 50.5 right now.
        assert_eq!(soc.update(at(1), 51.0, charge), 50.5);
        // Three minutes later: half a percent more, still reported as 51.
        let mut estimate = 0.0;
        for s in 2..=181 {
            estimate = soc.update(at(s), 51.0, charge);
        }
        assert!((estimate - 51.0).abs() < 0.01, "{estimate}");
        // It can't leave the reported value's band, however long it charges.
        for s in 182..=600 {
            estimate = soc.update(at(s), 51.0, charge);
        }
        assert!((estimate - 51.5).abs() < 1e-9, "{estimate}");
    }

    #[test]
    fn starts_over_after_a_gap_or_a_jump() {
        let mut soc = SocEstimator::new(WattHours(32_000.0));
        soc.update(at(0), 50.0, Watts(0.0));
        soc.update(at(1), 51.0, Watts(0.0));
        assert_eq!(soc.update(at(100), 51.0, Watts(0.0)), 51.0, "gap");
        assert_eq!(soc.update(at(101), 60.0, Watts(0.0)), 60.0, "jump");
    }

    #[test]
    fn fractional_reports_pass_through() {
        let mut soc = SocEstimator::new(WattHours(10_000.0));
        soc.update(at(0), 50.4, Watts(5000.0));
        assert_eq!(soc.update(at(1), 50.5, Watts(5000.0)), 50.5);
    }
}
