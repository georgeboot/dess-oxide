//! Weather from Open-Meteo's KNMI Harmonie model (`knmi_seamless`), per
//! 15-minute slot: the forecast, and history from the historical-forecast
//! archive (which stitches together the first hours of past model runs, so
//! models train on the same kind of data they predict from).
//!
//! Free for non-commercial use.

use std::collections::BTreeMap;

use anyhow::Context;
use dess_core::Slot;
use dess_core::weather::Weather;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::config::LocationConfig;

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const HISTORY_URL: &str = "https://historical-forecast-api.open-meteo.com/v1/forecast";
const VARIABLES: &str = "shortwave_radiation,direct_normal_irradiance,diffuse_radiation,\
                         temperature_2m,relative_humidity_2m,wind_speed_10m";
/// The first day KNMI Harmonie NL is in the historical-forecast archive.
pub const HISTORY_START: Date = jiff::civil::date(2024, 7, 1);

#[derive(Deserialize)]
struct Response {
    minutely_15: Series,
}

#[derive(Deserialize)]
struct Series {
    time: Vec<String>,
    shortwave_radiation: Vec<Option<f64>>,
    direct_normal_irradiance: Vec<Option<f64>>,
    diffuse_radiation: Vec<Option<f64>>,
    temperature_2m: Vec<Option<f64>>,
    relative_humidity_2m: Vec<Option<f64>>,
    wind_speed_10m: Vec<Option<f64>>,
}

/// The forecast for the next three days.
pub async fn forecast(
    client: &reqwest::Client,
    location: LocationConfig,
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    fetch(
        client,
        FORECAST_URL,
        location,
        &[("forecast_days", "3".to_owned())],
    )
    .await
}

/// What the model forecast at the time, for `[start, end]` (whole days).
pub async fn history(
    client: &reqwest::Client,
    location: LocationConfig,
    start: Date,
    end: Date,
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    fetch(
        client,
        HISTORY_URL,
        location,
        &[
            ("start_date", start.to_string()),
            ("end_date", end.to_string()),
        ],
    )
    .await
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    location: LocationConfig,
    extra: &[(&str, String)],
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    let mut query = vec![
        ("latitude", location.latitude.to_string()),
        ("longitude", location.longitude.to_string()),
        ("minutely_15", VARIABLES.to_owned()),
        ("models", "knmi_seamless".to_owned()),
        ("wind_speed_unit", "ms".to_owned()),
        ("timezone", "GMT".to_owned()),
    ];
    query.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    let response: Response = client
        .get(url)
        .query(&query)
        .send()
        .await
        .context("requesting Open-Meteo")?
        .error_for_status()
        .context("Open-Meteo answered with an error")?
        .json()
        .await
        .context("parsing Open-Meteo's response")?;
    parse(&response.minutely_15)
}

/// Radiation values average the preceding 15 minutes, so the value stamped
/// 12:15 belongs to the slot starting at 12:00. Temperature, humidity and
/// wind are instantaneous; the value at the slot's end is close enough.
fn parse(series: &Series) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    let mut out = BTreeMap::new();
    for (i, time) in series.time.iter().enumerate() {
        let get = |values: &[Option<f64>]| values.get(i).copied().flatten();
        let (Some(ghi), Some(dni), Some(dhi), Some(temperature), Some(humidity), Some(wind)) = (
            get(&series.shortwave_radiation),
            get(&series.direct_normal_irradiance),
            get(&series.diffuse_radiation),
            get(&series.temperature_2m),
            get(&series.relative_humidity_2m),
            get(&series.wind_speed_10m),
        ) else {
            continue;
        };
        let end = time
            .parse::<DateTime>()
            .with_context(|| format!("parsing Open-Meteo time {time}"))?
            .to_zoned(TimeZone::UTC)?
            .timestamp();
        let slot = Slot::containing(end - jiff::SignedDuration::from_secs(1));
        out.insert(
            slot,
            Weather {
                ghi: ghi.max(0.0),
                dni: dni.max(0.0),
                dhi: dhi.max(0.0),
                temperature,
                humidity,
                wind,
            },
        );
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
            shortwave_radiation: vec![Some(500.0), None],
            direct_normal_irradiance: vec![Some(400.0), Some(1.0)],
            diffuse_radiation: vec![Some(200.0), Some(1.0)],
            temperature_2m: vec![Some(15.0), Some(15.0)],
            relative_humidity_2m: vec![Some(80.0), Some(80.0)],
            wind_speed_10m: vec![Some(3.0), Some(3.0)],
        };
        let parsed = parse(&series).unwrap();
        assert_eq!(parsed.len(), 1, "incomplete slots are skipped");
        let (slot, weather) = parsed.iter().next().unwrap();
        assert_eq!(
            slot.start(),
            "2026-09-27T12:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_eq!(weather.ghi, 500.0);
    }
}
