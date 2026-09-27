//! Day-ahead prices from Nord Pool's public data portal.
//!
//! The endpoint is unofficial but widely used (HA's own Nord Pool
//! integration uses it). Anonymous access covers roughly the last two months;
//! the next day's prices appear around 12:55 CET, and until then the
//! endpoint answers `204 No Content`.

use std::collections::HashMap;

use anyhow::{Context, bail};
use dess_core::Slot;
use jiff::Timestamp;
use jiff::civil::Date;
use serde::Deserialize;

const URL: &str = "https://dataportal-api.nordpoolgroup.com/api/DayAheadPrices";

/// Prices for one delivery day, in €/MWh per 15-minute slot.
#[derive(Debug, Clone, PartialEq)]
pub struct DayPrices {
    /// `false` while Nord Pool still marks the results preliminary.
    pub is_final: bool,
    pub slots: Vec<(Slot, f64)>,
}

pub struct NordPool {
    client: reqwest::Client,
    area: String,
}

impl NordPool {
    pub fn new(client: reqwest::Client, area: &str) -> Self {
        Self {
            client,
            area: area.to_owned(),
        }
    }

    /// Prices for the CET delivery day `date`, or `None` if not published yet.
    pub async fn day(&self, date: Date) -> anyhow::Result<Option<DayPrices>> {
        let date = date.to_string();
        let response = self
            .client
            .get(URL)
            .query(&[
                ("date", date.as_str()),
                ("market", "DayAhead"),
                ("deliveryArea", self.area.as_str()),
                ("currency", "EUR"),
            ])
            .send()
            .await
            .context("requesting Nord Pool prices")?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(None),
            status if status.is_success() => {
                let body = response.bytes().await.context("reading Nord Pool prices")?;
                parse(&body, &self.area).map(Some)
            }
            status => bail!("Nord Pool answered {status} for {date}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    multi_area_entries: Vec<Entry>,
    #[serde(default)]
    area_states: Vec<AreaState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    delivery_start: Timestamp,
    delivery_end: Timestamp,
    #[serde(rename = "entryPerArea")]
    per_area: HashMap<String, Option<f64>>,
}

#[derive(Deserialize)]
struct AreaState {
    state: String,
    areas: Vec<String>,
}

fn parse(body: &[u8], area: &str) -> anyhow::Result<DayPrices> {
    let response: Response = serde_json::from_slice(body).context("parsing Nord Pool prices")?;
    let is_final = response
        .area_states
        .iter()
        .any(|s| s.state == "Final" && s.areas.iter().any(|a| a == area));
    let mut slots = Vec::new();
    for entry in response.multi_area_entries {
        let Some(price) = entry.per_area.get(area).copied().flatten() else {
            continue;
        };
        // Entries are 15 minutes since the SDAC switched in October 2025;
        // hourly entries (older history) cover four slots.
        let mut slot = Slot::containing(entry.delivery_start);
        while slot.start() < entry.delivery_end {
            slots.push((slot, price));
            slot = slot.next();
        }
    }
    Ok(DayPrices { is_final, slots })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quarter_hours() {
        let body = include_bytes!("../tests/fixtures/nordpool_day_ahead.json");
        let day = parse(body, "NL").unwrap();
        assert!(day.is_final);
        assert_eq!(day.slots.len(), 4);
        assert_eq!(
            day.slots[0].0.start(),
            "2026-09-26T22:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(day.slots[0].1, 192.07);
    }

    #[test]
    fn splits_hourly_entries() {
        let body = br#"{"multiAreaEntries": [{"deliveryStart": "2025-06-01T10:00:00Z",
            "deliveryEnd": "2025-06-01T11:00:00Z", "entryPerArea": {"NL": -5.0}}]}"#;
        let day = parse(body, "NL").unwrap();
        assert!(!day.is_final);
        assert_eq!(day.slots.len(), 4);
        assert!(day.slots.iter().all(|(_, p)| *p == -5.0));
    }

    #[test]
    fn ignores_other_areas() {
        let body = br#"{"multiAreaEntries": [{"deliveryStart": "2025-06-01T10:00:00Z",
            "deliveryEnd": "2025-06-01T10:15:00Z", "entryPerArea": {"BE": 50.0}}]}"#;
        assert!(parse(body, "NL").unwrap().slots.is_empty());
    }
}
