//! 15-minute market time units.
//!
//! A slot is identified by its UTC start. Local time only matters at the
//! edges (day boundaries, display), so DST needs no special handling here: a
//! DST day simply contains 92 or 100 slots.

use jiff::{SignedDuration, Timestamp};

/// Length of one slot in seconds (the day-ahead market's 15-minute MTU).
pub const SLOT_SECONDS: i64 = 900;

/// A 15-minute interval `[start, start + 15 min)`, aligned to the quarter hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot {
    start_unix: i64,
}

impl Slot {
    /// The slot containing `ts`.
    pub fn containing(ts: Timestamp) -> Self {
        Self {
            start_unix: ts.as_second().div_euclid(SLOT_SECONDS) * SLOT_SECONDS,
        }
    }

    /// The slot starting at `unix` seconds, which must be quarter-hour aligned.
    pub fn from_start_unix(unix: i64) -> Option<Self> {
        (unix.rem_euclid(SLOT_SECONDS) == 0).then_some(Self { start_unix: unix })
    }

    pub fn start_unix(self) -> i64 {
        self.start_unix
    }

    pub fn start(self) -> Timestamp {
        Timestamp::from_second(self.start_unix).expect("slot start is within jiff's range")
    }

    pub fn end(self) -> Timestamp {
        self.next().start()
    }

    #[must_use]
    pub fn next(self) -> Self {
        Self {
            start_unix: self.start_unix + SLOT_SECONDS,
        }
    }

    /// Time from `ts` until the end of this slot (zero if `ts` is past it).
    pub fn remaining(self, ts: Timestamp) -> SignedDuration {
        (self.end().duration_since(ts)).max(SignedDuration::ZERO)
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.start().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn containing_floors_to_quarter_hour() {
        let slot = Slot::containing(ts("2026-09-27T11:14:59Z"));
        assert_eq!(slot.start(), ts("2026-09-27T11:00:00Z"));
        assert_eq!(slot.end(), ts("2026-09-27T11:15:00Z"));
        assert_eq!(Slot::containing(ts("2026-09-27T11:15:00Z")), slot.next());
    }

    #[test]
    fn from_start_unix_requires_alignment() {
        assert!(Slot::from_start_unix(900).is_some());
        assert!(Slot::from_start_unix(901).is_none());
    }

    #[test]
    fn dst_days_have_92_or_100_slots() {
        let tz = jiff::tz::TimeZone::get("Europe/Amsterdam").unwrap();
        let count = |day: &str| {
            let date: jiff::civil::Date = day.parse().unwrap();
            let start = date.to_zoned(tz.clone()).unwrap().timestamp();
            let end = date
                .tomorrow()
                .unwrap()
                .to_zoned(tz.clone())
                .unwrap()
                .timestamp();
            let mut slot = Slot::containing(start);
            let mut n = 0;
            while slot.start() < end {
                n += 1;
                slot = slot.next();
            }
            n
        };
        assert_eq!(count("2026-03-29"), 92);
        assert_eq!(count("2026-10-25"), 100);
        assert_eq!(count("2026-09-27"), 96);
    }
}
