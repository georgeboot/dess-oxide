//! OpenAmber: the heat pump's energy split into heating and hot water.
//!
//! Home Assistant's state history has OpenAmber's mode, its legionella flag
//! and the heat pump meter's readings, but only for as long as the recorder
//! keeps states (10 days by default). So dess-oxide imports it every hour
//! into its own table: a backfill takes whatever is still there, and from
//! then on the split history keeps growing. It also reads when the next
//! legionella run is due.

use std::sync::Arc;
use std::time::Duration;

use dess_core::Slot;
use dess_core::heat_pump_modes::{self, Mode};
use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::OpenAmberEntities;
use crate::homeassistant::{self, Connection, Endpoint};
use crate::planning::lock;
use crate::run::Shared;

/// Imported up to here (a slot boundary), in the settings table.
const CURSOR: &str = "heat_pump_modes_until";
/// How far back the first import looks.
const BACKFILL: SignedDuration = SignedDuration::from_hours(24 * 60);
/// History is fetched in pieces this long, to keep the messages small.
const CHUNK: SignedDuration = SignedDuration::from_hours(6);

pub async fn run(shared: Arc<Shared>, client: reqwest::Client, mut stop: watch::Receiver<bool>) {
    let Some(entities) = shared.config.openamber() else {
        return;
    };
    let Some(endpoint) = Endpoint::resolve(&shared.config) else {
        warn!("openamber_device is set, but there's no Home Assistant to read it from");
        return;
    };
    let Some(meter) = shared.config.history.heat_pump.clone() else {
        return;
    };
    let mut wait = Duration::from_secs(90);
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|stopping| *stopping) => return,
        }
        wait = Duration::from_secs(3600);
        match import(&shared, &endpoint, &entities, &meter).await {
            Ok(0) => {}
            Ok(slots) => info!(
                slots,
                "split the heat pump's energy into heating and hot water"
            ),
            Err(error) => warn!("importing OpenAmber's history: {error:#}"),
        }
        let next = homeassistant::state(&client, &endpoint, &entities.next_legionella).await;
        match next {
            Ok(state) => {
                let at = state.and_then(|s| parse_datetime(&s, &shared.tz));
                *shared.next_legionella.lock().expect("lock poisoned") = at;
            }
            Err(error) => warn!("reading the next legionella run: {error:#}"),
        }
    }
}

/// HA datetime states are ISO 8601 with an offset; older ones local time.
fn parse_datetime(state: &str, tz: &jiff::tz::TimeZone) -> Option<Timestamp> {
    state.parse::<Timestamp>().ok().or_else(|| {
        let local: jiff::civil::DateTime = state.parse().ok()?;
        Some(local.to_zoned(tz.clone()).ok()?.timestamp())
    })
}

/// Imports and splits everything since the cursor; returns the slots stored.
async fn import(
    shared: &Shared,
    endpoint: &Endpoint,
    entities: &OpenAmberEntities,
    meter: &str,
) -> anyhow::Result<usize> {
    let now = Timestamp::now();
    let until = Slot::containing(now - SignedDuration::from_mins(5)).start();
    let cursor = lock(&shared.store)
        .setting(CURSOR)?
        .and_then(|s| s.parse::<Timestamp>().ok())
        .unwrap_or_else(|| Slot::containing(now - BACKFILL).start());
    if cursor >= until {
        return Ok(0);
    }
    let mut connection = Connection::connect(endpoint).await?;
    let ids = [meter, entities.mode.as_str(), entities.legionella.as_str()];
    let mut stored = 0;
    let mut start = cursor;
    while start < until {
        let end = (start + CHUNK).min(until);
        let history = homeassistant::state_history(&mut connection, &ids, start, end).await?;
        let energy: Vec<(Timestamp, f64)> = history
            .get(meter)
            .into_iter()
            .flatten()
            .filter_map(|(at, s)| Some((*at, s.parse::<f64>().ok()?)))
            .collect();
        let mode: Vec<(Timestamp, Option<Mode>)> = history
            .get(&entities.mode)
            .into_iter()
            .flatten()
            .map(|(at, s)| (*at, Mode::from_openamber(s)))
            .collect();
        let legionella: Vec<(Timestamp, bool)> = history
            .get(&entities.legionella)
            .into_iter()
            .flatten()
            .map(|(at, s)| (*at, s == "on"))
            .collect();
        let slots = heat_pump_modes::split(&energy, &mode, &legionella, start, end);
        tokio::task::block_in_place(|| {
            let mut store = lock(&shared.store);
            store.save_heat_pump_modes(&slots)?;
            store.set_setting(CURSOR, &end.to_string())
        })?;
        stored += slots.len();
        start = end;
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ha_datetimes() {
        let tz = jiff::tz::TimeZone::get("Europe/Amsterdam").unwrap();
        let utc = parse_datetime("2026-10-04T09:00:00+00:00", &tz).unwrap();
        let local = parse_datetime("2026-10-04 11:00:00", &tz).unwrap();
        assert_eq!(utc, local);
        assert!(parse_datetime("unknown", &tz).is_none());
    }
}
