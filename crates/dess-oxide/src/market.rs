//! What the price forecast is made from: the hourly price history
//! (EnergyZero, the EPEX NL day-ahead prices), weather at points across the
//! Netherlands and Germany (Open-Meteo: its forecast archive for training,
//! its forecast for the days ahead), and, with a key, NED's forecasts of
//! Dutch wind and solar production.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use dess_core::{EurPerKwh, Slot};
use dess_models::price::{self, PriceHour};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::planning::lock;
use crate::run::Shared;
use crate::store::Store;
use jiff::civil::Date;
use jiff::{Timestamp, ToSpan};
use serde::Deserialize;
use serde_json::Value;

/// EpexPredictor's points for NL (north-east, Amsterdam, the Belgian
/// border, offshore by Texel): wind, temperature, sun, pressure, humidity.
const NL_POINTS: [(f64, f64); 4] = [(52.69, 6.11), (52.36, 4.90), (50.51, 5.41), (53.36, 4.98)];
/// Germany's wind (Schleswig-Holstein, the Weser coast, Brandenburg) and sun
/// (Bavaria, the centre): its renewables move the coupled NL price too.
const DE_POINTS: [(f64, f64); 5] = [
    (54.4, 9.2),
    (53.6, 8.1),
    (52.8, 12.5),
    (48.8, 11.0),
    (51.0, 9.5),
];
const VARIABLES: &str =
    "wind_speed_80m,temperature_2m,shortwave_radiation,pressure_msl,relative_humidity_2m";

/// Hourly EPEX NL prices over `[from, until)`, €/MWh, by hour (unix seconds).
pub async fn prices(
    client: &reqwest::Client,
    from: Date,
    until: Date,
) -> anyhow::Result<BTreeMap<i64, f64>> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "Prices")]
        prices: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "readingDate")]
        reading_date: String,
        price: f64,
    }
    let mut out = BTreeMap::new();
    let mut start = from;
    while start < until {
        let end = start.checked_add(31.days())?.min(until);
        let response: Response = client
            .get("https://api.energyzero.nl/v1/energyprices")
            .query(&[
                ("fromDate", format!("{start}T00:00:00.000Z")),
                ("tillDate", format!("{}T23:59:59.999Z", end.yesterday()?)),
                ("interval", "4".to_owned()),
                ("usageType", "1".to_owned()),
                ("inclBtw", "false".to_owned()),
            ])
            .send()
            .await
            .context("asking EnergyZero for prices")?
            .error_for_status()?
            .json()
            .await
            .context("reading EnergyZero's prices")?;
        for row in response.prices {
            let at: Timestamp = row.reading_date.parse()?;
            out.insert(at.as_second(), row.price * 1000.0);
        }
        start = end;
    }
    Ok(out)
}

/// Weather inputs by hour: the forecast archive for `[start, end]` (whole
/// days), or with `start = None` the forecast for the coming week.
pub async fn weather(
    client: &reqwest::Client,
    range: Option<(Date, Date)>,
) -> anyhow::Result<BTreeMap<i64, Vec<f64>>> {
    let points: Vec<(f64, f64)> = NL_POINTS.iter().chain(&DE_POINTS).copied().collect();
    let join = |f: fn(&(f64, f64)) -> f64| {
        points
            .iter()
            .map(|p| f(p).to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let (url, mut query) = match range {
        Some((start, end)) => (
            "https://historical-forecast-api.open-meteo.com/v1/forecast",
            vec![
                ("start_date", start.to_string()),
                ("end_date", end.to_string()),
            ],
        ),
        None => (
            "https://api.open-meteo.com/v1/forecast",
            vec![("forecast_days", "7".to_owned())],
        ),
    };
    query.extend([
        ("latitude", join(|p| p.0)),
        ("longitude", join(|p| p.1)),
        ("hourly", VARIABLES.to_owned()),
        ("wind_speed_unit", "ms".to_owned()),
        ("timezone", "GMT".to_owned()),
    ]);
    let body: Value = client
        .get(url)
        .query(&query)
        .send()
        .await
        .context("asking Open-Meteo for the market weather")?
        .error_for_status()?
        .json()
        .await?;
    let locations = body
        .as_array()
        .context("Open-Meteo: expected one result per point")?;
    if locations.len() != points.len() {
        bail!(
            "Open-Meteo returned {} points, not {}",
            locations.len(),
            points.len()
        );
    }
    let series = |loc: &Value, name: &str| -> Vec<Option<f64>> {
        loc["hourly"][name]
            .as_array()
            .map(|a| a.iter().map(Value::as_f64).collect())
            .unwrap_or_default()
    };
    let times: Vec<i64> = locations[0]["hourly"]["time"]
        .as_array()
        .context("Open-Meteo: no times")?
        .iter()
        .filter_map(|t| {
            let civil: jiff::civil::DateTime = t.as_str()?.parse().ok()?;
            Some(
                civil
                    .to_zoned(jiff::tz::TimeZone::UTC)
                    .ok()?
                    .timestamp()
                    .as_second(),
            )
        })
        .collect();
    let columns: Vec<Vec<Option<f64>>> = locations
        .iter()
        .enumerate()
        .flat_map(|(i, loc)| {
            let names: &[&str] = if i < NL_POINTS.len() {
                &[
                    "wind_speed_80m",
                    "temperature_2m",
                    "shortwave_radiation",
                    "pressure_msl",
                    "relative_humidity_2m",
                ]
            } else {
                &["wind_speed_80m", "shortwave_radiation"]
            };
            names.iter().map(|n| series(loc, n)).collect::<Vec<_>>()
        })
        .collect();
    let mut out = BTreeMap::new();
    for (row, &hour) in times.iter().enumerate() {
        let values: Option<Vec<f64>> = columns
            .iter()
            .map(|c| c.get(row).copied().flatten())
            .collect();
        if let Some(values) = values {
            out.insert(hour, values);
        }
    }
    Ok(out)
}

/// NED's hourly forecasts of Dutch production for the (UTC) days
/// `[from, until)`, MW: wind on land, wind at sea, and solar. Past hours keep
/// their last forecast. (NED takes plain dates: timestamps get a 403.)
pub async fn ned(
    client: &reqwest::Client,
    key: &str,
    from: Date,
    until: Date,
) -> anyhow::Result<BTreeMap<i64, [f64; 3]>> {
    const TYPES: [u8; 3] = [1, 17, 2];
    let mut out: BTreeMap<i64, [f64; 3]> = BTreeMap::new();
    let mut seen: BTreeMap<i64, u8> = BTreeMap::new();
    for (column, kind) in TYPES.iter().enumerate() {
        let mut start = from;
        while start < until {
            // Seven days of hours fit one page.
            let end = start.checked_add(7.days())?.min(until);
            let body: Value = client
                .get("https://api.ned.nl/v1/utilizations")
                .header("X-AUTH-TOKEN", key)
                .header("Accept", "application/json")
                .query(&[
                    ("point", "0".to_owned()),
                    ("type", kind.to_string()),
                    ("granularity", "5".to_owned()),
                    ("granularitytimezone", "0".to_owned()),
                    ("classification", "1".to_owned()),
                    ("activity", "1".to_owned()),
                    ("validfrom[after]", start.to_string()),
                    ("validfrom[strictly_before]", end.to_string()),
                    ("itemsPerPage", "200".to_owned()),
                ])
                .send()
                .await
                .context("asking NED for its forecasts")?
                .error_for_status()?
                .json()
                .await?;
            let items = body
                .as_array()
                .cloned()
                .or_else(|| body["hydra:member"].as_array().cloned())
                .unwrap_or_default();
            for item in items {
                let (Some(at), Some(kw)) = (item["validfrom"].as_str(), item["capacity"].as_f64())
                else {
                    continue;
                };
                let hour = at.parse::<Timestamp>()?.as_second();
                out.entry(hour).or_default()[column] = kw / 1000.0;
                *seen.entry(hour).or_default() |= 1 << column;
            }
            start = end;
        }
    }
    // Only hours with all three.
    out.retain(|hour, _| seen.get(hour) == Some(&0b111));
    Ok(out)
}

/// The model trains on this much history.
const TRAINING_DAYS: i64 = 180;
/// And forecasts this far ahead (the weather forecast's reach).
const FORECAST_DAYS: i64 = 7;
/// Up to here the weather archive is stored (a date), in the settings table.
const WEATHER_CURSOR: &str = "market_weather_archived_until";

/// Keeps the market data current, trains the price model nightly, and
/// publishes its forecast for the hours without published prices.
pub async fn run(shared: Arc<Shared>, client: reqwest::Client, mut stop: watch::Receiver<bool>) {
    let key = shared
        .config
        .ned_api_key
        .clone()
        .filter(|k| !k.trim().is_empty());
    let mut trained: Option<Date> = None;
    let mut wait = Duration::from_secs(60);
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|stopping| *stopping) => return,
        }
        wait = Duration::from_secs(3600);
        if let Err(error) = update(&shared, &client, key.as_deref()).await {
            warn!("updating the market data: {error:#}");
            continue;
        }
        let now = Timestamp::now();
        let today = now.to_zoned(shared.tz.clone()).date();
        // Nightly (and once after a start), off the async threads.
        if trained.is_none()
            || (trained != Some(today) && now.to_zoned(shared.tz.clone()).hour() >= 3)
        {
            train(&shared, key.is_some()).await;
            trained = Some(today);
        }
        publish_forecast(&shared, key.is_some());
    }
}

/// Fetches what's new: prices, the weather archive up to yesterday, the
/// weather forecast, and NED's forecasts (past and coming).
async fn update(
    shared: &Shared,
    client: &reqwest::Client,
    key: Option<&str>,
) -> anyhow::Result<()> {
    let utc = jiff::tz::TimeZone::UTC;
    let today = Timestamp::now().to_zoned(utc.clone()).date();
    let backfill = today.checked_sub((TRAINING_DAYS + 20).days())?;

    let last_price = lock(&shared.store)
        .market_prices(0)?
        .keys()
        .next_back()
        .map(|&h| Timestamp::from_second(h).map(|t| t.to_zoned(utc.clone()).date()))
        .transpose()?;
    let from = last_price.map_or(backfill, |d| d.max(backfill));
    let prices = prices(client, from, today.checked_add(2.days())?).await?;
    tokio::task::block_in_place(|| lock(&shared.store).save_market_prices(&prices))?;

    let archived: Option<Date> = lock(&shared.store)
        .setting(WEATHER_CURSOR)?
        .and_then(|d| d.parse().ok());
    let start = archived.map_or(backfill, |d| d.max(backfill));
    let yesterday = today.yesterday()?;
    if start <= yesterday {
        let history = weather(client, Some((start, yesterday))).await?;
        tokio::task::block_in_place(|| {
            let mut store = lock(&shared.store);
            store.save_market_inputs("weather", &history)?;
            store.set_setting(WEATHER_CURSOR, &today.to_string())
        })?;
    }
    let now = Timestamp::now().as_second();
    let coming: BTreeMap<i64, Vec<f64>> = weather(client, None)
        .await?
        .into_iter()
        .filter(|(hour, _)| *hour >= now - 3600)
        .collect();
    tokio::task::block_in_place(|| lock(&shared.store).save_market_inputs("weather", &coming))?;

    if let Some(key) = key {
        // The last two days again (their forecasts settle), and the week ahead.
        let have = lock(&shared.store).market_inputs("ned", 0)?;
        let first = have
            .keys()
            .next_back()
            .map(|&h| Timestamp::from_second(h).map(|t| t.to_zoned(utc.clone()).date()))
            .transpose()?
            .map_or(backfill, |d| {
                d.checked_sub(2.days()).unwrap_or(d).max(backfill)
            });
        let ned = ned(
            client,
            key,
            first,
            today.checked_add((FORECAST_DAYS + 1).days())?,
        )
        .await?;
        let ned: BTreeMap<i64, Vec<f64>> = ned.into_iter().map(|(h, v)| (h, v.to_vec())).collect();
        tokio::task::block_in_place(|| lock(&shared.store).save_market_inputs("ned", &ned))?;
    }
    Ok(())
}

/// The hours to train on or forecast: inputs (weather, then NED when used)
/// with their price level; prices where known.
fn hours(
    store: &Store,
    with_ned: bool,
    from: i64,
) -> anyhow::Result<(Vec<PriceHour>, BTreeMap<i64, f64>)> {
    let prices = store.market_prices(from - 20 * 86_400)?;
    let weather = store.market_inputs("weather", from)?;
    let ned = if with_ned {
        store.market_inputs("ned", from)?
    } else {
        BTreeMap::new()
    };
    let hours = weather
        .iter()
        .filter_map(|(&hour, w)| {
            let mut inputs = w.clone();
            if with_ned {
                inputs.extend(ned.get(&hour)?);
            }
            Some(PriceHour {
                hour,
                inputs,
                level: price::level(&prices, hour)?,
                price: prices.get(&hour).copied().unwrap_or(f64::NAN),
            })
        })
        .collect();
    Ok((hours, prices))
}

async fn train(shared: &Arc<Shared>, with_ned: bool) {
    let now = Timestamp::now();
    let for_fit = Arc::clone(shared);
    let result = tokio::task::spawn_blocking(move || {
        let from = now.as_second() - TRAINING_DAYS * 86_400;
        let (hours, _) = hours(&lock(&for_fit.store), with_ned, from)?;
        let known: Vec<PriceHour> = hours.into_iter().filter(|h| h.price.is_finite()).collect();
        anyhow::Ok(price::fit(&known, &for_fit.tz))
    })
    .await;
    let fit = match result {
        Ok(Ok(Some(fit))) => fit,
        Ok(Ok(None)) => return info!("not enough market history to train the price model yet"),
        Ok(Err(error)) => return warn!("training the price model: {error:#}"),
        Err(error) => return error!(%error, "price training panicked"),
    };
    let promoted = fit.improves();
    info!(
        hours = fit.hours,
        error_ct = format!("{:.2}", fit.validation_mae / 10.0),
        recent_median_ct = format!("{:.2}", fit.baseline_mae / 10.0),
        with_ned,
        promoted,
        "trained the price model"
    );
    let metrics = serde_json::json!({
        "hours": fit.hours,
        "validation_mae_eur_mwh": fit.validation_mae,
        "baseline_mae_eur_mwh": fit.baseline_mae,
        "with_ned": with_ned,
    });
    if let Err(error) =
        lock(&shared.store).save_model("price", now, &serde_json::json!({}), &metrics, promoted)
    {
        error!(%error, "storing the price model's results");
    }
    shared
        .price_model
        .send_replace(promoted.then(|| Arc::new(fit.model)));
}

/// The model's prices for the coming hours without a published price, per
/// slot, for the planner.
fn publish_forecast(shared: &Shared, with_ned: bool) {
    let Some(model) = shared.price_model.borrow().clone() else {
        shared
            .price_forecast
            .send_replace(Arc::new(BTreeMap::new()));
        return;
    };
    let now = Timestamp::now().as_second();
    let forecast =
        tokio::task::block_in_place(|| hours(&lock(&shared.store), with_ned, now - 3600));
    let (hours, known) = match forecast {
        Ok(f) => f,
        Err(error) => return warn!("forecasting prices: {error:#}"),
    };
    let mut slots = BTreeMap::new();
    for h in hours.iter().filter(|h| !known.contains_key(&h.hour)) {
        let Some(eur_per_mwh) = model.predict(h.hour, &h.inputs, h.level, &shared.tz) else {
            continue;
        };
        let Some(mut slot) = Slot::from_start_unix(h.hour) else {
            continue;
        };
        for _ in 0..4 {
            slots.insert(slot, EurPerKwh::from_eur_per_mwh(eur_per_mwh));
            slot = slot.next();
        }
    }
    shared.price_forecast.send_replace(Arc::new(slots));
}
