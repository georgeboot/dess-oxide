//! The page's language: Dutch or English, from Home Assistant's language
//! unless the `language` option picks one.

use jiff::Zoned;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    Nl,
}

impl Lang {
    /// From a language code such as HA's `nl` or `en-GB`.
    pub fn from_code(code: &str) -> Self {
        if code.trim().to_ascii_lowercase().starts_with("nl") {
            Self::Nl
        } else {
            Self::En
        }
    }

    /// The text in this language.
    pub fn t(self, en: &'static str, nl: &'static str) -> &'static str {
        match self {
            Self::En => en,
            Self::Nl => nl,
        }
    }

    /// `strftime` with weekday and month names in this language.
    pub fn strftime(self, at: &Zoned, format: &str) -> String {
        match self {
            Self::En => at.strftime(format).to_string(),
            Self::Nl => {
                const DAYS: [&str; 7] = ["ma", "di", "wo", "do", "vr", "za", "zo"];
                const MONTHS: [&str; 12] = [
                    "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov",
                    "dec",
                ];
                let day = DAYS[usize::from(at.weekday().to_monday_zero_offset().unsigned_abs())];
                let month = MONTHS[usize::from(at.month().unsigned_abs()) - 1];
                let format = format.replace("%a", day).replace("%b", month);
                at.strftime(&format).to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dutch_dates() {
        let at: Zoned = "2026-09-27T17:00:00+02:00[Europe/Amsterdam]"
            .parse()
            .unwrap();
        assert_eq!(Lang::Nl.strftime(&at, "%a %d %b %H:%M"), "zo 27 sep 17:00");
        assert_eq!(Lang::En.strftime(&at, "%a %d %b"), "Sun 27 Sep");
        assert_eq!(Lang::from_code("nl"), Lang::Nl);
        assert_eq!(Lang::from_code("en-GB"), Lang::En);
    }
}
