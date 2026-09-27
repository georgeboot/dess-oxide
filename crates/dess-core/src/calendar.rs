//! Dutch public holidays, which behave like Sundays for household load.

use jiff::civil::{Date, Weekday, date};

/// Easter Sunday (anonymous Gregorian algorithm).
#[allow(clippy::many_single_char_names, reason = "the names of the published algorithm")]
pub fn easter(year: i16) -> Date {
    let y = i32::from(year);
    let a = y % 19;
    let b = y / 100;
    let c = y % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    date(year, month as i8, day as i8)
}

/// Whether `day` is a Dutch public holiday most people have off.
pub fn is_holiday(day: Date) -> bool {
    let year = day.year();
    let easter = easter(year);
    let offset = |days: i64| easter.checked_add(jiff::Span::new().days(days)).ok();
    // King's Day moves to Saturday the 26th when the 27th is a Sunday.
    let kings_day = if date(year, 4, 27).weekday() == Weekday::Sunday {
        date(year, 4, 26)
    } else {
        date(year, 4, 27)
    };
    let fixed = [
        date(year, 1, 1),
        kings_day,
        date(year, 12, 25),
        date(year, 12, 26),
    ];
    let moving = [offset(1), offset(39), offset(50)]; // Easter Monday, Ascension, Whit Monday
    fixed.contains(&day) || moving.contains(&Some(day)) || day == easter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easter_dates() {
        assert_eq!(easter(2024), date(2024, 3, 31));
        assert_eq!(easter(2026), date(2026, 4, 5));
        assert_eq!(easter(2027), date(2027, 3, 28));
    }

    #[test]
    fn dutch_holidays() {
        assert!(is_holiday(date(2026, 4, 6)), "Easter Monday");
        assert!(is_holiday(date(2026, 5, 14)), "Ascension");
        assert!(is_holiday(date(2026, 5, 25)), "Whit Monday");
        assert!(
            is_holiday(date(2025, 4, 26)),
            "King's Day on a Saturday when the 27th is a Sunday"
        );
        assert!(!is_holiday(date(2026, 9, 28)));
    }
}
