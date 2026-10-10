//! Weather from Open-Meteo's KNMI Harmonie model (`knmi_seamless`), per
//! 15-minute slot: the forecast, and history from the historical-forecast
//! archive (which stitches together the first hours of past model runs, so
//! models train on the same kind of data they predict from).
//!
//! With it comes the irradiance two more weather models expect ([`OTHERS`]).
//! Over a year at two Dutch sites KNMI's sunshine for the next day was off
//! by 77 W/m² on average, ECMWF's by 56 and ICON's by 65, and KNMI was the
//! best of the day on one day in ten: the PV forecast weighs all three.
//!
//! Free for non-commercial use.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use dess_core::Slot;
use dess_core::solar::Irradiance;
use dess_core::weather::{OTHER_MODELS, Weather};
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use serde::Deserialize;
use tracing::warn;

use crate::config::LocationConfig;

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const HISTORY_URL: &str = "https://historical-forecast-api.open-meteo.com/v1/forecast";
const VARIABLES: &str = "shortwave_radiation,direct_normal_irradiance,diffuse_radiation,\
                         temperature_2m,relative_humidity_2m,wind_speed_10m";
const RADIATION: [&str; 3] = [
    "shortwave_radiation",
    "direct_normal_irradiance",
    "diffuse_radiation",
];
/// The other weather models, in [`Weather::others`]' order: ECMWF's IFS and
/// the German weather service's ICON.
pub const OTHERS: [&str; OTHER_MODELS] = ["ecmwf_ifs025", "icon_seamless"];
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

/// The other models' radiation: one list per variable and model, named
/// `<variable>_<model>`.
#[derive(Deserialize)]
struct OthersResponse {
    minutely_15: OthersSeries,
}

#[derive(Deserialize)]
struct OthersSeries {
    time: Vec<String>,
    #[serde(flatten)]
    values: HashMap<String, Vec<Option<f64>>>,
}

type Skies = BTreeMap<Slot, [Option<Irradiance>; OTHER_MODELS]>;

/// The forecast for the next three days.
pub async fn forecast(
    client: &reqwest::Client,
    location: LocationConfig,
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    let days = [("forecast_days", "3".to_owned())];
    let mut weather = fetch(client, FORECAST_URL, location, &days).await?;
    // Without the other models the forecast is still good: the PV forecast
    // then takes the first one's word.
    match others(client, FORECAST_URL, location, &days).await {
        Ok(skies) => merge(&mut weather, &skies),
        Err(error) => warn!("the other weather models' forecast: {error:#}"),
    }
    Ok(weather)
}

/// What the model forecast at the time, for `[start, end]` (whole days).
pub async fn history(
    client: &reqwest::Client,
    location: LocationConfig,
    start: Date,
    end: Date,
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    let days = [
        ("start_date", start.to_string()),
        ("end_date", end.to_string()),
    ];
    let mut weather = fetch(client, HISTORY_URL, location, &days).await?;
    // The archive is written once: days stored without the other models
    // would stay without them. So no archive until they answer.
    merge(
        &mut weather,
        &others(client, HISTORY_URL, location, &days).await?,
    );
    Ok(weather)
}

async fn get<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    location: LocationConfig,
    variables: &str,
    models: &str,
    extra: &[(&str, String)],
) -> anyhow::Result<T> {
    let mut query = vec![
        ("latitude", location.latitude.to_string()),
        ("longitude", location.longitude.to_string()),
        ("minutely_15", variables.to_owned()),
        ("models", models.to_owned()),
        ("wind_speed_unit", "ms".to_owned()),
        ("timezone", "GMT".to_owned()),
    ];
    query.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    client
        .get(url)
        .query(&query)
        .send()
        .await
        .context("requesting Open-Meteo")?
        .error_for_status()
        .context("Open-Meteo answered with an error")?
        .json()
        .await
        .context("parsing Open-Meteo's response")
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    location: LocationConfig,
    extra: &[(&str, String)],
) -> anyhow::Result<BTreeMap<Slot, Weather>> {
    let response: Response = get(client, url, location, VARIABLES, "knmi_seamless", extra).await?;
    parse(&response.minutely_15)
}

async fn others(
    client: &reqwest::Client,
    url: &str,
    location: LocationConfig,
    extra: &[(&str, String)],
) -> anyhow::Result<Skies> {
    let response: OthersResponse = get(
        client,
        url,
        location,
        &RADIATION.join(","),
        &OTHERS.join(","),
        extra,
    )
    .await?;
    parse_others(&response.minutely_15)
}

fn merge(weather: &mut BTreeMap<Slot, Weather>, skies: &Skies) {
    for (slot, w) in weather {
        if let Some(others) = skies.get(slot) {
            w.others = *others;
        }
    }
}

/// The slot a value stamped `time` belongs to: the one ending then.
fn slot_ending(time: &str) -> anyhow::Result<Slot> {
    let end = time
        .parse::<DateTime>()
        .with_context(|| format!("parsing Open-Meteo time {time}"))?
        .to_zoned(TimeZone::UTC)?
        .timestamp();
    Ok(Slot::containing(end - jiff::SignedDuration::from_secs(1)))
}

fn parse_others(series: &OthersSeries) -> anyhow::Result<Skies> {
    let mut out = BTreeMap::new();
    for (i, time) in series.time.iter().enumerate() {
        let sky = |model: &str| {
            let get = |variable: &str| {
                let values = series.values.get(&format!("{variable}_{model}"))?;
                values.get(i).copied().flatten().map(|v| v.max(0.0))
            };
            Some(Irradiance {
                ghi: get(RADIATION[0])?,
                dni: get(RADIATION[1])?,
                dhi: get(RADIATION[2])?,
            })
        };
        out.insert(slot_ending(time)?, OTHERS.map(sky));
    }
    Ok(out)
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
        out.insert(
            slot_ending(time)?,
            Weather {
                ghi: ghi.max(0.0),
                dni: dni.max(0.0),
                dhi: dhi.max(0.0),
                temperature,
                humidity,
                wind,
                others: [None; OTHER_MODELS],
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

    #[test]
    fn the_other_models_come_per_variable_and_model() {
        let response: OthersResponse = serde_json::from_str(
            r#"{"latitude": 52.3, "minutely_15": {
                "time": ["2026-09-27T12:15", "2026-09-27T12:30"],
                "shortwave_radiation_ecmwf_ifs025": [400.0, 410.0],
                "direct_normal_irradiance_ecmwf_ifs025": [300.0, null],
                "diffuse_radiation_ecmwf_ifs025": [150.0, 150.0],
                "shortwave_radiation_icon_seamless": [200.0, 210.0],
                "direct_normal_irradiance_icon_seamless": [50.0, 60.0],
                "diffuse_radiation_icon_seamless": [170.0, -1.0]
            }}"#,
        )
        .unwrap();
        let skies = parse_others(&response.minutely_15).unwrap();
        let at = |time: &str| skies[&Slot::containing(time.parse().unwrap())];
        let [ecmwf, icon] = at("2026-09-27T12:00:00Z");
        assert_eq!(ecmwf.unwrap().ghi, 400.0);
        assert_eq!(icon.unwrap().dni, 50.0);
        // A model with a value missing has no sky for that slot.
        let [ecmwf, icon] = at("2026-09-27T12:15:00Z");
        assert_eq!(ecmwf, None);
        assert_eq!(icon.unwrap().dhi, 0.0);
    }
}
