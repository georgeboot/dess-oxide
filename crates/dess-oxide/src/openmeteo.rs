//! Baseline PV forecast from Open-Meteo's KNMI Harmonie model, until M2's
//! learned PV model replaces it.
//!
//! Open-Meteo computes irradiance on a tilted plane (`global_tilted_irradiance`)
//! itself; the baseline is simply `kWp × GTI × performance ratio` per array.
//! Free for non-commercial use; this makes one request per array per plan.

use std::collections::BTreeMap;

use anyhow::Context;
use dess_core::{Slot, Watts};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::config::{LocationConfig, PvArrayConfig};

const URL: &str = "https://api.open-meteo.com/v1/forecast";
/// System losses: inverter, wiring, soiling, temperature (a typical 0.85).
const PERFORMANCE_RATIO: f64 = 0.85;

#[derive(Deserialize)]
struct Response {
    minutely_15: Series,
}

#[derive(Deserialize)]
struct Series {
    time: Vec<String>,
    global_tilted_irradiance: Vec<Option<f64>>,
}

/// Expected AC-coupled PV power per slot over the next few days.
pub async fn pv_forecast(
    client: &reqwest::Client,
    location: LocationConfig,
    arrays: &[PvArrayConfig],
) -> anyhow::Result<BTreeMap<Slot, Watts>> {
    let mut total: BTreeMap<Slot, Watts> = BTreeMap::new();
    for array in arrays {
        let response: Response = client
            .get(URL)
            .query(&[
                ("latitude", location.latitude.to_string()),
                ("longitude", location.longitude.to_string()),
                ("minutely_15", "global_tilted_irradiance".to_owned()),
                ("tilt", array.tilt.to_string()),
                // Open-Meteo: 0 = south, −90 = east, 90 = west.
                ("azimuth", (array.azimuth - 180.0).to_string()),
                ("models", "knmi_seamless".to_owned()),
                ("forecast_days", "3".to_owned()),
                ("timezone", "GMT".to_owned()),
            ])
            .send()
            .await
            .context("requesting Open-Meteo")?
            .error_for_status()
            .context("Open-Meteo answered with an error")?
            .json()
            .await
            .context("parsing Open-Meteo's response")?;
        for (slot, gti) in parse(&response.minutely_15)? {
            *total.entry(slot).or_default() += Watts(array.kwp * gti * PERFORMANCE_RATIO);
        }
    }
    Ok(total)
}

/// Values are averages over the preceding 15 minutes, so the value stamped
/// 12:15 belongs to the slot starting at 12:00.
fn parse(series: &Series) -> anyhow::Result<Vec<(Slot, f64)>> {
    let mut out = Vec::with_capacity(series.time.len());
    for (time, value) in series.time.iter().zip(&series.global_tilted_irradiance) {
        let Some(gti) = value else { continue };
        let end = time
            .parse::<DateTime>()
            .with_context(|| format!("parsing Open-Meteo time {time}"))?
            .to_zoned(TimeZone::UTC)?
            .timestamp();
        let slot = Slot::containing(end - jiff::SignedDuration::from_secs(1));
        out.push((slot, gti.max(0.0)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_belong_to_the_preceding_slot() {
        let series = Series {
            time: vec!["2026-09-27T12:15".into(), "2026-09-27T12:30".into()],
            global_tilted_irradiance: vec![Some(500.0), None],
        };
        let parsed = parse(&series).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].0.start(),
            "2026-09-27T12:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );
    }
}
