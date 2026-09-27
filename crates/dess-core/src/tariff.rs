//! Retail prices from day-ahead spot prices.
//!
//! Every component is date-effective: a value applies from its date (local
//! calendar date) until the next change. That keeps history correct in
//! backtests when taxes or supplier markups change.

use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::slot::Slot;
use crate::units::EurPerKwh;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TariffError {
    #[error("{component} has no value on or before {date}")]
    NotCovered { component: &'static str, date: Date },
    #[error("{component} has no values")]
    Empty { component: &'static str },
}

/// A value that changes on given dates and holds until the next change.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule<T> {
    component: &'static str,
    changes: Vec<(Date, T)>,
}

impl<T: Copy> Schedule<T> {
    pub fn new(
        component: &'static str,
        changes: impl IntoIterator<Item = (Date, T)>,
    ) -> Result<Self, TariffError> {
        let mut changes: Vec<_> = changes.into_iter().collect();
        if changes.is_empty() {
            return Err(TariffError::Empty { component });
        }
        changes.sort_by_key(|(date, _)| *date);
        Ok(Self { component, changes })
    }

    /// The value in effect on `date`.
    pub fn at(&self, date: Date) -> Result<T, TariffError> {
        let index = self.changes.partition_point(|(from, _)| *from <= date);
        index
            .checked_sub(1)
            .map(|i| self.changes[i].1)
            .ok_or(TariffError::NotCovered {
                component: self.component,
                date,
            })
    }
}

/// Buy and sell price for one slot, all-in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotPrices {
    pub buy: EurPerKwh,
    pub sell: EurPerKwh,
}

/// A Dutch dynamic contract. These are the prices of the *marginal* kWh,
/// which is what decisions need.
///
/// ```text
/// buy  = (spot + markup_buy + energy_tax) × (1 + vat)
/// sell = (spot + markup_sell) × (1 + vat if vat_on_export)
/// ```
///
/// While exports are netted against imports (salderen, until 2027-01-01),
/// the energy tax only counts for the year's *net* import:
/// - **Net importer:** every exported kWh cancels an imported one, so it earns
///   the buy price's tax and VAT back:
///   `sell = (spot + markup_sell + energy_tax) × (1 + vat)`.
/// - **Net exporter:** the surplus earns only the feed-in rate, and one more
///   imported kWh just shrinks the surplus. The tax isn't at stake on either
///   side: `buy = (spot + markup_buy) × (1 + vat)`, and `sell` as above.
///
/// `markup_sell` is added as-is: positive when the supplier nets its markup
/// too, negative for a feed-in fee.
#[derive(Debug, Clone, PartialEq)]
pub struct Tariff {
    pub vat: Schedule<f64>,
    pub energy_tax: Schedule<EurPerKwh>,
    pub markup_buy: Schedule<EurPerKwh>,
    pub markup_sell: Schedule<EurPerKwh>,
    /// The last day on which exports are netted against imports.
    pub net_metering_until: Option<Date>,
    /// Whether exports exceed imports over the netting period.
    pub net_exporter: bool,
    /// Whether the supplier pays VAT on exports once net metering has ended.
    pub vat_on_export: bool,
    pub time_zone: TimeZone,
}

impl Tariff {
    pub fn prices(&self, slot: Slot, spot: EurPerKwh) -> Result<SlotPrices, TariffError> {
        let date = slot.start().to_zoned(self.time_zone.clone()).date();
        let vat = self.vat.at(date)?;
        let tax = self.energy_tax.at(date)?;
        let markup_buy = self.markup_buy.at(date)?;
        let markup_sell = self.markup_sell.at(date)?;
        let export_vat = if self.vat_on_export { vat } else { 0.0 };
        let netted = self.net_metering_until.is_some_and(|until| date <= until);
        let (buy, sell) = match (netted, self.net_exporter) {
            (true, false) => (
                (spot + markup_buy + tax).0 * (1.0 + vat),
                (spot + markup_sell + tax).0 * (1.0 + vat),
            ),
            (true, true) => (
                (spot + markup_buy).0 * (1.0 + vat),
                (spot + markup_sell).0 * (1.0 + export_vat),
            ),
            (false, _) => (
                (spot + markup_buy + tax).0 * (1.0 + vat),
                (spot + markup_sell).0 * (1.0 + export_vat),
            ),
        };
        Ok(SlotPrices {
            buy: EurPerKwh(buy),
            sell: EurPerKwh(sell),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    /// George's contract, as configured in DAO.
    fn george() -> Tariff {
        Tariff {
            vat: Schedule::new("vat", [(date(2023, 1, 1), 0.21)]).unwrap(),
            energy_tax: Schedule::new(
                "energy_tax",
                [
                    (date(2025, 1, 1), EurPerKwh(0.10154)),
                    (date(2026, 1, 1), EurPerKwh(0.09161)),
                ],
            )
            .unwrap(),
            markup_buy: Schedule::new("markup_buy", [(date(2025, 12, 14), EurPerKwh(0.01504))])
                .unwrap(),
            markup_sell: Schedule::new("markup_sell", [(date(2025, 12, 14), EurPerKwh(0.01504))])
                .unwrap(),
            net_metering_until: Some(date(2026, 12, 31)),
            net_exporter: false,
            vat_on_export: false,
            time_zone: TimeZone::get("Europe/Amsterdam").unwrap(),
        }
    }

    fn slot(s: &str) -> Slot {
        Slot::containing(s.parse().unwrap())
    }

    #[test]
    fn schedule_picks_the_latest_change() {
        let s = Schedule::new("x", [(date(2026, 1, 1), 2), (date(2025, 1, 1), 1)]).unwrap();
        assert_eq!(s.at(date(2025, 6, 1)), Ok(1));
        assert_eq!(s.at(date(2026, 1, 1)), Ok(2));
        assert!(s.at(date(2024, 12, 31)).is_err());
    }

    #[test]
    fn net_metered_export_earns_the_buy_price() {
        let p = george()
            .prices(
                slot("2026-09-27T12:00:00Z"),
                EurPerKwh::from_eur_per_mwh(100.0),
            )
            .unwrap();
        let expected = (0.100 + 0.01504 + 0.09161) * 1.21;
        assert!((p.buy.0 - expected).abs() < 1e-12);
        assert!((p.sell.0 - expected).abs() < 1e-12);
    }

    #[test]
    fn export_after_net_metering_earns_spot() {
        let mut tariff = george();
        tariff.markup_sell =
            Schedule::new("markup_sell", [(date(2025, 1, 1), EurPerKwh(-0.02))]).unwrap();
        let spot = EurPerKwh::from_eur_per_mwh(80.0);
        let p = tariff.prices(slot("2027-01-01T12:00:00Z"), spot).unwrap();
        assert!((p.sell.0 - 0.06).abs() < 1e-12);
        assert!(p.buy.0 > p.sell.0 + 0.1);
    }

    #[test]
    fn the_local_date_decides() {
        // 2026-12-31 23:30 UTC is already 2027-01-01 in Amsterdam: no more netting.
        let p = george()
            .prices(
                slot("2026-12-31T23:30:00Z"),
                EurPerKwh::from_eur_per_mwh(100.0),
            )
            .unwrap();
        assert!((p.sell.0 - (0.100 + 0.01504)).abs() < 1e-12);
    }

    #[test]
    fn a_net_exporter_has_no_tax_at_stake() {
        let mut tariff = george();
        tariff.net_exporter = true;
        let p = tariff
            .prices(
                slot("2026-09-27T12:00:00Z"),
                EurPerKwh::from_eur_per_mwh(-20.0),
            )
            .unwrap();
        assert!((p.buy.0 - (-0.020 + 0.01504) * 1.21).abs() < 1e-12);
        assert!(
            p.sell.0 < 0.0,
            "exporting at a negative spot price costs money"
        );
        // After net metering ends, the tax is back on imports.
        let p = tariff
            .prices(
                slot("2027-01-02T12:00:00Z"),
                EurPerKwh::from_eur_per_mwh(-20.0),
            )
            .unwrap();
        assert!(p.buy.0 > 0.08);
    }
}
