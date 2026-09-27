//! Turns roughly once-a-second system samples into 15-minute energy records.
//!
//! Integration is sample-and-hold: each sample's values are held until the
//! next sample, split exactly at slot boundaries. Intervals longer than the
//! configured maximum gap are not integrated at all, and show up as reduced
//! coverage instead of invented energy.

use jiff::{SignedDuration, Timestamp};

use crate::slot::{SLOT_SECONDS, Slot};
use crate::units::{WattHours, Watts};

/// One reading of the whole system.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub at: Timestamp,
    /// Battery state of charge as reported by the BMS, 0–100 %.
    pub soc_pct: f64,
    /// Battery DC power, positive = charging.
    pub battery: Watts,
    /// Battery voltage.
    pub battery_voltage: f64,
    /// Net grid power summed over the phases, positive = import. Dutch meters
    /// net across phases, so the sum is what gets billed.
    pub grid: Watts,
    /// AC-coupled PV, on the inverter output and on the grid side.
    pub pv_ac: Watts,
    /// DC-coupled PV (MPPTs).
    pub pv_dc: Watts,
    /// Loads on the inverter output, backed up during an outage.
    pub load_out: Watts,
    /// Loads between the grid meter and the inverters, not backed up (e.g. an EV charger).
    pub load_in: Watts,
    /// Inverter/charger AC power: AC-in minus AC-out summed over the phases.
    /// Positive = converting AC to DC (charging).
    pub inverter_ac: Watts,
    pub grid_connected: bool,
    /// Cerbo relays 1 and 2, `true` = closed (energised).
    pub relays: [Option<bool>; 2],
    /// ESS grid setpoint in effect: the volatile override if set, else the setting.
    pub setpoint: Option<Watts>,
}

/// Energy totals for one 15-minute slot.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotRecord {
    pub slot: Slot,
    /// Seconds of the slot covered by integrated samples, `0..=900`.
    pub covered_seconds: f64,
    pub grid_import: WattHours,
    pub grid_export: WattHours,
    pub pv_ac: WattHours,
    pub pv_dc: WattHours,
    pub load_out: WattHours,
    pub load_in: WattHours,
    pub battery_charge: WattHours,
    pub battery_discharge: WattHours,
    pub inverter_ac_to_dc: WattHours,
    pub inverter_dc_to_ac: WattHours,
    pub soc_start: f64,
    pub soc_end: f64,
    pub soc_min: f64,
    pub soc_max: f64,
    pub grid_lost_seconds: f64,
    pub relay_closed_seconds: [f64; 2],
    /// Integral of the setpoint over the seconds it was known; divide by
    /// `setpoint_seconds` for the mean.
    pub setpoint_integral: WattHours,
    pub setpoint_seconds: f64,
}

impl SlotRecord {
    fn open(slot: Slot, soc_pct: f64) -> Self {
        Self {
            slot,
            covered_seconds: 0.0,
            grid_import: WattHours::ZERO,
            grid_export: WattHours::ZERO,
            pv_ac: WattHours::ZERO,
            pv_dc: WattHours::ZERO,
            load_out: WattHours::ZERO,
            load_in: WattHours::ZERO,
            battery_charge: WattHours::ZERO,
            battery_discharge: WattHours::ZERO,
            inverter_ac_to_dc: WattHours::ZERO,
            inverter_dc_to_ac: WattHours::ZERO,
            soc_start: soc_pct,
            soc_end: soc_pct,
            soc_min: soc_pct,
            soc_max: soc_pct,
            grid_lost_seconds: 0.0,
            relay_closed_seconds: [0.0; 2],
            setpoint_integral: WattHours::ZERO,
            setpoint_seconds: 0.0,
        }
    }

    /// Mean setpoint over the part of the slot where it was known.
    pub fn setpoint_mean(&self) -> Option<Watts> {
        (self.setpoint_seconds > 0.0)
            .then(|| Watts(self.setpoint_integral.0 * 3600.0 / self.setpoint_seconds))
    }

    /// Fraction of the slot covered by samples.
    pub fn coverage(&self) -> f64 {
        self.covered_seconds / SLOT_SECONDS as f64
    }

    fn observe_soc(&mut self, soc_pct: f64) {
        self.soc_end = soc_pct;
        self.soc_min = self.soc_min.min(soc_pct);
        self.soc_max = self.soc_max.max(soc_pct);
    }

    /// Adds `s` held for `seconds`.
    fn accumulate(&mut self, s: &Sample, seconds: f64) {
        self.covered_seconds += seconds;
        self.grid_import += s.grid.positive_part().over_seconds(seconds);
        self.grid_export += s.grid.negative_part().over_seconds(seconds);
        self.pv_ac += s.pv_ac.over_seconds(seconds);
        self.pv_dc += s.pv_dc.over_seconds(seconds);
        self.load_out += s.load_out.over_seconds(seconds);
        self.load_in += s.load_in.over_seconds(seconds);
        self.battery_charge += s.battery.positive_part().over_seconds(seconds);
        self.battery_discharge += s.battery.negative_part().over_seconds(seconds);
        self.inverter_ac_to_dc += s.inverter_ac.positive_part().over_seconds(seconds);
        self.inverter_dc_to_ac += s.inverter_ac.negative_part().over_seconds(seconds);
        if !s.grid_connected {
            self.grid_lost_seconds += seconds;
        }
        for (closed_seconds, relay) in self.relay_closed_seconds.iter_mut().zip(s.relays) {
            if relay == Some(true) {
                *closed_seconds += seconds;
            }
        }
        if let Some(setpoint) = s.setpoint {
            self.setpoint_integral += setpoint.over_seconds(seconds);
            self.setpoint_seconds += seconds;
        }
    }
}

/// Accumulates samples into slot records.
#[derive(Debug)]
pub struct Recorder {
    max_gap: SignedDuration,
    previous: Option<Sample>,
    open: Option<SlotRecord>,
}

impl Recorder {
    /// `max_gap` is the longest interval between two samples that is still
    /// integrated. Longer gaps count as missing data.
    pub fn new(max_gap: SignedDuration) -> Self {
        Self {
            max_gap,
            previous: None,
            open: None,
        }
    }

    /// Feeds the next sample and returns any slots it completed, oldest first.
    pub fn push(&mut self, sample: Sample) -> Vec<SlotRecord> {
        let mut completed = Vec::new();

        if let Some(previous) = self.previous.take() {
            let gap = sample.at.duration_since(previous.at);
            if gap > SignedDuration::ZERO && gap <= self.max_gap {
                self.integrate(&previous, sample.at, &mut completed);
            }
        }

        let slot = Slot::containing(sample.at);
        if self.open.as_ref().is_none_or(|open| open.slot != slot) {
            completed.extend(self.open.replace(SlotRecord::open(slot, sample.soc_pct)));
        }
        if let Some(open) = &mut self.open {
            open.observe_soc(sample.soc_pct);
        }

        self.previous = Some(sample);
        completed
    }

    /// Closes the open slot, e.g. on shutdown. Its record is partial.
    pub fn flush(&mut self) -> Option<SlotRecord> {
        self.previous = None;
        self.open.take()
    }

    /// Holds `held` from its timestamp until `until`, splitting at slot boundaries.
    fn integrate(&mut self, held: &Sample, until: Timestamp, completed: &mut Vec<SlotRecord>) {
        let mut from = held.at;
        while from < until {
            let slot = Slot::containing(from);
            let to = until.min(slot.end());
            if self.open.as_ref().is_none_or(|open| open.slot != slot) {
                completed.extend(self.open.replace(SlotRecord::open(slot, held.soc_pct)));
            }
            let open = self.open.as_mut().expect("opened above");
            open.observe_soc(held.soc_pct);
            open.accumulate(held, to.duration_since(from).as_secs_f64());
            from = to;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn sample(ts: Timestamp) -> Sample {
        Sample {
            at: ts,
            soc_pct: 50.0,
            battery: Watts(1000.0),
            battery_voltage: 52.0,
            grid: Watts(-400.0),
            pv_ac: Watts(2000.0),
            pv_dc: Watts::ZERO,
            load_out: Watts(600.0),
            load_in: Watts::ZERO,
            inverter_ac: Watts(1050.0),
            grid_connected: true,
            relays: [Some(false), Some(true)],
            setpoint: Some(Watts(50.0)),
        }
    }

    fn feed(recorder: &mut Recorder, start: Timestamp, count: i64, step_s: i64) -> Vec<SlotRecord> {
        (0..count)
            .flat_map(|i| recorder.push(sample(start + SignedDuration::from_secs(i * step_s))))
            .collect()
    }

    #[test]
    fn full_slot_integrates_exactly() {
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        let done = feed(&mut recorder, at("2026-09-27T11:00:00Z"), 902, 1);
        assert_eq!(done.len(), 1);
        let r = &done[0];
        assert_eq!(r.slot.start(), at("2026-09-27T11:00:00Z"));
        assert!((r.covered_seconds - 900.0).abs() < 1e-9);
        assert!((r.battery_charge.0 - 250.0).abs() < 1e-9);
        assert!((r.grid_export.0 - 100.0).abs() < 1e-9);
        assert_eq!(r.grid_import, WattHours::ZERO);
        assert!((r.relay_closed_seconds[1] - 900.0).abs() < 1e-9);
        assert!(r.relay_closed_seconds[0].abs() < 1e-9);
        assert!((r.setpoint_mean().unwrap().0 - 50.0).abs() < 1e-9);
    }

    #[test]
    fn interval_is_split_at_the_slot_boundary() {
        let mut recorder = Recorder::new(SignedDuration::from_secs(30));
        assert!(recorder.push(sample(at("2026-09-27T11:14:50Z"))).is_empty());
        let done = recorder.push(sample(at("2026-09-27T11:15:10Z")));
        assert_eq!(done.len(), 1);
        assert!((done[0].covered_seconds - 10.0).abs() < 1e-9);
        let open = recorder.flush().unwrap();
        assert_eq!(open.slot.start(), at("2026-09-27T11:15:00Z"));
        assert!((open.covered_seconds - 10.0).abs() < 1e-9);
    }

    #[test]
    fn long_gaps_are_not_integrated() {
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        recorder.push(sample(at("2026-09-27T11:00:00Z")));
        recorder.push(sample(at("2026-09-27T11:05:00Z")));
        recorder.push(sample(at("2026-09-27T11:05:01Z")));
        let open = recorder.flush().unwrap();
        assert!((open.covered_seconds - 1.0).abs() < 1e-9);
    }

    #[test]
    fn soc_start_is_the_value_entering_the_slot() {
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        let mut s = sample(at("2026-09-27T11:14:58Z"));
        s.soc_pct = 40.0;
        recorder.push(s);
        let mut s = sample(at("2026-09-27T11:15:02Z"));
        s.soc_pct = 41.0;
        recorder.push(s);
        let open = recorder.flush().unwrap();
        assert_eq!((open.soc_start, open.soc_end), (40.0, 41.0));
        assert_eq!((open.soc_min, open.soc_max), (40.0, 41.0));
    }

    proptest! {
        /// Energy is conserved across slot splits, and coverage never exceeds a slot.
        #[test]
        fn energy_is_conserved(steps in prop::collection::vec(1i64..=12, 1..400), offset in 0i64..900) {
            let max_gap = SignedDuration::from_secs(10);
            let mut recorder = Recorder::new(max_gap);
            let mut t = at("2026-09-27T11:00:00Z") + SignedDuration::from_secs(offset);
            let mut expected_seconds = 0.0;
            let mut records = recorder.push(sample(t));
            for step in steps {
                t += SignedDuration::from_secs(step);
                if step <= 10 {
                    expected_seconds += step as f64;
                }
                records.extend(recorder.push(sample(t)));
            }
            records.extend(recorder.flush());
            let covered: f64 = records.iter().map(|r| r.covered_seconds).sum();
            let charged: f64 = records.iter().map(|r| r.battery_charge.0).sum();
            prop_assert!((covered - expected_seconds).abs() < 1e-6);
            prop_assert!((charged - 1000.0 * expected_seconds / 3600.0).abs() < 1e-6);
            for r in &records {
                prop_assert!(r.covered_seconds <= 900.0 + 1e-9);
            }
        }
    }
}
