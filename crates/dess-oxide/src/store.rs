//! `SQLite` storage in the app's `/data` directory.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use dess_core::efficiency::BinStats;
use dess_core::planner::{PlannedSlot, SlotForecast};
use dess_core::record::SlotRecord;
use dess_core::weather::Weather;
use dess_core::{EurPerKwh, Slot, Watts};
use rusqlite::{Connection, params};

/// Schema migrations, applied in order; `PRAGMA user_version` counts how many ran.
const MIGRATIONS: &[&str] = &[
    r"
    CREATE TABLE slot_measurements (
        slot_start            INTEGER PRIMARY KEY, -- unix seconds, UTC, quarter-hour aligned
        covered_seconds       REAL NOT NULL,       -- seconds of the slot backed by samples
        grid_import_wh        REAL NOT NULL,
        grid_export_wh        REAL NOT NULL,
        pv_ac_wh              REAL NOT NULL,
        pv_dc_wh              REAL NOT NULL,
        load_out_wh           REAL NOT NULL,       -- loads on the inverter output (backed up)
        load_in_wh            REAL NOT NULL,       -- loads before the inverters (not backed up)
        battery_charge_wh     REAL NOT NULL,       -- DC side
        battery_discharge_wh  REAL NOT NULL,       -- DC side
        inverter_ac_to_dc_wh  REAL NOT NULL,       -- AC side of the inverter/chargers
        inverter_dc_to_ac_wh  REAL NOT NULL,
        soc_start             REAL NOT NULL,
        soc_end               REAL NOT NULL,
        soc_min               REAL NOT NULL,
        soc_max               REAL NOT NULL,
        grid_lost_seconds     REAL NOT NULL,
        relay1_closed_seconds REAL NOT NULL,
        relay2_closed_seconds REAL NOT NULL,
        setpoint_integral_wh  REAL NOT NULL,       -- divide by setpoint_seconds/3600 for the mean
        setpoint_seconds      REAL NOT NULL,
        updated_at            INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE efficiency_bins (
        day         INTEGER NOT NULL, -- days since the Unix epoch, UTC
        bin         INTEGER NOT NULL, -- round(inverter AC power / 100 W), positive = charging
        n           INTEGER NOT NULL,
        sum_ac      REAL NOT NULL,
        sum_dc      REAL NOT NULL,
        sum_ac2     REAL NOT NULL,
        sum_dc2     REAL NOT NULL,
        sum_ac_dc   REAL NOT NULL,
        sum_voltage REAL NOT NULL,
        PRIMARY KEY (day, bin)
    ) STRICT;
",
    r"
    CREATE TABLE prices (
        slot_start  INTEGER PRIMARY KEY, -- unix seconds, UTC
        eur_per_mwh REAL NOT NULL,       -- day-ahead spot price
        is_final    INTEGER NOT NULL,    -- 0 while the exchange marks it preliminary
        source      TEXT NOT NULL,
        fetched_at  INTEGER NOT NULL
    ) STRICT;
",
    r"
    -- Every plan, with the forecasts it was made from. Doubles as the archive
    -- for judging forecast accuracy per lead time.
    CREATE TABLE plans (
        planned_at    INTEGER NOT NULL, -- unix seconds
        slot_start    INTEGER NOT NULL,
        load_w        REAL NOT NULL,    -- forecast
        pv_w          REAL NOT NULL,    -- forecast, if PV is on
        buy           REAL NOT NULL,    -- €/kWh
        sell          REAL NOT NULL,
        estimated     INTEGER NOT NULL, -- price is an estimate
        battery_ac_w  REAL NOT NULL,    -- planned, positive = charging
        grid_w        REAL NOT NULL,    -- planned, positive = import
        pv_on         INTEGER NOT NULL,
        soc_end       REAL NOT NULL,
        stored_value  REAL NOT NULL,    -- €/kWh of the last stored kWh
        PRIMARY KEY (planned_at, slot_start)
    ) STRICT;
",
    r"
    -- Hourly energy from Home Assistant's long-term statistics.
    CREATE TABLE ha_hourly (
        entity     TEXT NOT NULL,
        hour_start INTEGER NOT NULL, -- unix seconds
        kwh        REAL NOT NULL,
        PRIMARY KEY (entity, hour_start)
    ) STRICT;
",
    r"
    -- Weather per slot: the forecast for future slots, the historical-forecast
    -- archive for the past. Once a slot is past, forecasts no longer touch it.
    CREATE TABLE weather (
        slot_start  INTEGER PRIMARY KEY,
        ghi         REAL NOT NULL, -- W/m²
        dni         REAL NOT NULL,
        dhi         REAL NOT NULL,
        temperature REAL NOT NULL, -- °C
        humidity    REAL NOT NULL, -- %
        wind        REAL NOT NULL, -- m/s
        kind        TEXT NOT NULL, -- 'forecast' or 'history'
        issued_at   INTEGER NOT NULL
    ) STRICT;
",
];

pub struct Store {
    conn: Connection,
}

/// One recorded slot, with what was planned and forecast for it.
#[derive(Debug, Clone, PartialEq)]
pub struct HistorySlot {
    pub slot: Slot,
    /// Measured means over the slot.
    pub grid: Watts,
    pub load: Watts,
    pub pv: Watts,
    pub soc_end: f64,
    /// The ESS setpoint in effect (whoever set it; DAO during the rollout).
    pub setpoint: Option<Watts>,
    /// dess-oxide's plan for the slot, made when it started.
    pub planned_grid: Option<Watts>,
    pub forecast_load: Option<Watts>,
    pub forecast_pv: Option<Watts>,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let applied: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let tx = conn.transaction()?;
        for (version, sql) in (1..).zip(MIGRATIONS).skip(usize::try_from(applied)?) {
            tx.execute_batch(sql)
                .with_context(|| format!("applying migration {version}"))?;
            tx.pragma_update(None, "user_version", version)?;
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Saves a slot record. A record for the same slot (e.g. from before a
    /// restart) is merged: energies and durations add up.
    pub fn save_slot(&self, r: &SlotRecord, now_unix: i64) -> anyhow::Result<()> {
        self.conn.execute(
            r"INSERT INTO slot_measurements VALUES
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
              ON CONFLICT (slot_start) DO UPDATE SET
                covered_seconds       = covered_seconds + excluded.covered_seconds,
                grid_import_wh        = grid_import_wh + excluded.grid_import_wh,
                grid_export_wh        = grid_export_wh + excluded.grid_export_wh,
                pv_ac_wh              = pv_ac_wh + excluded.pv_ac_wh,
                pv_dc_wh              = pv_dc_wh + excluded.pv_dc_wh,
                load_out_wh           = load_out_wh + excluded.load_out_wh,
                load_in_wh            = load_in_wh + excluded.load_in_wh,
                battery_charge_wh     = battery_charge_wh + excluded.battery_charge_wh,
                battery_discharge_wh  = battery_discharge_wh + excluded.battery_discharge_wh,
                inverter_ac_to_dc_wh  = inverter_ac_to_dc_wh + excluded.inverter_ac_to_dc_wh,
                inverter_dc_to_ac_wh  = inverter_dc_to_ac_wh + excluded.inverter_dc_to_ac_wh,
                soc_end               = excluded.soc_end,
                soc_min               = min(soc_min, excluded.soc_min),
                soc_max               = max(soc_max, excluded.soc_max),
                grid_lost_seconds     = grid_lost_seconds + excluded.grid_lost_seconds,
                relay1_closed_seconds = relay1_closed_seconds + excluded.relay1_closed_seconds,
                relay2_closed_seconds = relay2_closed_seconds + excluded.relay2_closed_seconds,
                setpoint_integral_wh  = setpoint_integral_wh + excluded.setpoint_integral_wh,
                setpoint_seconds      = setpoint_seconds + excluded.setpoint_seconds,
                updated_at            = excluded.updated_at",
            params![
                r.slot.start_unix(),
                r.covered_seconds,
                r.grid_import.0,
                r.grid_export.0,
                r.pv_ac.0,
                r.pv_dc.0,
                r.load_out.0,
                r.load_in.0,
                r.battery_charge.0,
                r.battery_discharge.0,
                r.inverter_ac_to_dc.0,
                r.inverter_dc_to_ac.0,
                r.soc_start,
                r.soc_end,
                r.soc_min,
                r.soc_max,
                r.grid_lost_seconds,
                r.relay_closed_seconds[0],
                r.relay_closed_seconds[1],
                r.setpoint_integral.0,
                r.setpoint_seconds,
                now_unix,
            ],
        )?;
        Ok(())
    }

    /// Stores spot prices in €/MWh, replacing earlier values for the same slots.
    pub fn save_prices(
        &mut self,
        prices: &[(Slot, f64)],
        is_final: bool,
        source: &str,
        now_unix: i64,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR REPLACE INTO prices VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for (slot, price) in prices {
                insert.execute(params![
                    slot.start_unix(),
                    price,
                    is_final,
                    source,
                    now_unix
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Spot prices for `[from, until)`.
    pub fn prices(&self, from: Slot, until: Slot) -> anyhow::Result<BTreeMap<Slot, EurPerKwh>> {
        let mut query = self.conn.prepare_cached(
            "SELECT slot_start, eur_per_mwh FROM prices WHERE slot_start >= ?1 AND slot_start < ?2",
        )?;
        let rows = query.query_map([from.start_unix(), until.start_unix()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut prices = BTreeMap::new();
        for row in rows {
            let (start, price) = row?;
            if let Some(slot) = Slot::from_start_unix(start) {
                prices.insert(slot, EurPerKwh::from_eur_per_mwh(price));
            }
        }
        Ok(prices)
    }

    /// How many final prices are stored for `[from, until)`.
    pub fn final_price_count(&self, from: Slot, until: Slot) -> anyhow::Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM prices WHERE slot_start >= ?1 AND slot_start < ?2 AND is_final",
            [from.start_unix(), until.start_unix()],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }

    /// Mean power of all loads per recorded slot since `from`, for slots with
    /// at least half their time covered.
    pub fn load_history(&self, from: Slot) -> anyhow::Result<Vec<(Slot, Watts)>> {
        let mut query = self.conn.prepare_cached(
            "SELECT slot_start, (load_out_wh + load_in_wh) * 3600.0 / covered_seconds
             FROM slot_measurements WHERE slot_start >= ?1 AND covered_seconds >= 450 ORDER BY slot_start",
        )?;
        let rows = query.query_map([from.start_unix()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut history = Vec::new();
        for row in rows {
            let (start, watts) = row?;
            if let Some(slot) = Slot::from_start_unix(start) {
                history.push((slot, Watts(watts)));
            }
        }
        Ok(history)
    }

    /// Stores a plan and the forecasts it was made from.
    pub fn save_plan(
        &mut self,
        planned_at: i64,
        slots: &[PlannedSlot],
        forecasts: &[SlotForecast],
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO plans VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for (s, f) in slots.iter().zip(forecasts) {
                insert.execute(params![
                    planned_at,
                    s.slot.start_unix(),
                    f.load.0,
                    f.pv.0,
                    s.prices.buy.0,
                    s.prices.sell.0,
                    s.estimated_price,
                    s.battery_ac.0,
                    s.grid.0,
                    s.pv_on,
                    s.soc_end,
                    s.stored_energy_value.0,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes plans made before `keep_since` (unix seconds), except each
    /// plan's first slot, which records what was decided for that slot.
    pub fn prune_plans(&self, keep_since: i64) -> anyhow::Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM plans WHERE planned_at < ?1 AND slot_start > planned_at",
            [keep_since],
        )?)
    }

    /// Recorded slots since `from` (at least a minute covered), joined with the
    /// plan made at the start of each.
    pub fn history(&self, from: Slot) -> anyhow::Result<Vec<HistorySlot>> {
        let mut query = self.conn.prepare_cached(
            "SELECT m.slot_start,
                    (m.grid_import_wh - m.grid_export_wh) * 3600.0 / m.covered_seconds,
                    (m.load_out_wh + m.load_in_wh) * 3600.0 / m.covered_seconds,
                    (m.pv_ac_wh + m.pv_dc_wh) * 3600.0 / m.covered_seconds,
                    m.soc_end,
                    CASE WHEN m.setpoint_seconds > 0 THEN m.setpoint_integral_wh * 3600.0 / m.setpoint_seconds END,
                    p.grid_w, p.load_w, p.pv_w
             FROM slot_measurements m
             LEFT JOIN (SELECT slot_start, grid_w, load_w, pv_w, min(planned_at)
                        FROM plans WHERE slot_start <= planned_at GROUP BY slot_start) p
               USING (slot_start)
             WHERE m.slot_start >= ?1 AND m.covered_seconds > 60
             ORDER BY m.slot_start",
        )?;
        let rows = query.query_map([from.start_unix()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                HistorySlot {
                    slot: Slot::containing(jiff::Timestamp::UNIX_EPOCH),
                    grid: Watts(row.get(1)?),
                    load: Watts(row.get(2)?),
                    pv: Watts(row.get(3)?),
                    soc_end: row.get(4)?,
                    setpoint: row.get::<_, Option<f64>>(5)?.map(Watts),
                    planned_grid: row.get::<_, Option<f64>>(6)?.map(Watts),
                    forecast_load: row.get::<_, Option<f64>>(7)?.map(Watts),
                    forecast_pv: row.get::<_, Option<f64>>(8)?.map(Watts),
                },
            ))
        })?;
        let mut history = Vec::new();
        for row in rows {
            let (start, mut slot) = row?;
            if let Some(s) = Slot::from_start_unix(start) {
                slot.slot = s;
                history.push(slot);
            }
        }
        Ok(history)
    }

    /// Stores hourly energy (kWh) per HA entity, replacing earlier values.
    pub fn save_ha_hourly(
        &mut self,
        rows: &[(String, jiff::Timestamp, f64)],
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR REPLACE INTO ha_hourly VALUES (?1, ?2, ?3)")?;
            for (entity, start, kwh) in rows {
                insert.execute(params![entity, start.as_second(), kwh])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The start of the latest imported hour for `entity`.
    pub fn last_ha_hour(&self, entity: &str) -> anyhow::Result<Option<jiff::Timestamp>> {
        let last: Option<i64> = self.conn.query_row(
            "SELECT max(hour_start) FROM ha_hourly WHERE entity = ?1",
            [entity],
            |row| row.get(0),
        )?;
        last.map(jiff::Timestamp::from_second)
            .transpose()
            .map_err(Into::into)
    }

    /// Hourly kWh per entity since `from`: `hour → entity → kWh`.
    pub fn ha_hourly(
        &self,
        entities: &[&str],
        from: jiff::Timestamp,
    ) -> anyhow::Result<BTreeMap<i64, std::collections::HashMap<String, f64>>> {
        let mut out: BTreeMap<i64, std::collections::HashMap<String, f64>> = BTreeMap::new();
        let mut query = self.conn.prepare_cached(
            "SELECT entity, hour_start, kwh FROM ha_hourly WHERE entity = ?1 AND hour_start >= ?2",
        )?;
        for entity in entities {
            let rows = query.query_map(params![entity, from.as_second()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?;
            for row in rows {
                let (entity, hour, kwh) = row?;
                out.entry(hour).or_default().insert(entity, kwh);
            }
        }
        Ok(out)
    }

    /// Stores weather. Forecasts only replace slots from `now` on; history
    /// replaces anything.
    pub fn save_weather(
        &mut self,
        weather: &BTreeMap<Slot, Weather>,
        is_history: bool,
        now: jiff::Timestamp,
    ) -> anyhow::Result<()> {
        let current = Slot::containing(now);
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO weather VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (slot, w) in weather {
                if !is_history && *slot < current {
                    continue;
                }
                insert.execute(params![
                    slot.start_unix(),
                    w.ghi,
                    w.dni,
                    w.dhi,
                    w.temperature,
                    w.humidity,
                    w.wind,
                    if is_history { "history" } else { "forecast" },
                    now.as_second(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Weather for `[from, until)`.
    #[allow(dead_code, reason = "read by the M2 training pipeline, next")]
    pub fn weather(&self, from: Slot, until: Slot) -> anyhow::Result<BTreeMap<Slot, Weather>> {
        let mut query = self.conn.prepare_cached(
            "SELECT slot_start, ghi, dni, dhi, temperature, humidity, wind FROM weather
             WHERE slot_start >= ?1 AND slot_start < ?2",
        )?;
        let rows = query.query_map([from.start_unix(), until.start_unix()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Weather {
                    ghi: row.get(1)?,
                    dni: row.get(2)?,
                    dhi: row.get(3)?,
                    temperature: row.get(4)?,
                    humidity: row.get(5)?,
                    wind: row.get(6)?,
                },
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (start, weather) = row?;
            if let Some(slot) = Slot::from_start_unix(start) {
                out.insert(slot, weather);
            }
        }
        Ok(out)
    }

    /// The last slot with archived (historical) weather.
    pub fn last_weather_history(&self) -> anyhow::Result<Option<Slot>> {
        let last: Option<i64> = self.conn.query_row(
            "SELECT max(slot_start) FROM weather WHERE kind = 'history'",
            [],
            |row| row.get(0),
        )?;
        Ok(last.and_then(Slot::from_start_unix))
    }

    /// Adds steady-state conversion samples to the day's bins.
    pub fn save_efficiency_bins(
        &mut self,
        day: i64,
        bins: &BTreeMap<i32, BinStats>,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                r"INSERT INTO efficiency_bins VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                  ON CONFLICT (day, bin) DO UPDATE SET
                    n           = n + excluded.n,
                    sum_ac      = sum_ac + excluded.sum_ac,
                    sum_dc      = sum_dc + excluded.sum_dc,
                    sum_ac2     = sum_ac2 + excluded.sum_ac2,
                    sum_dc2     = sum_dc2 + excluded.sum_dc2,
                    sum_ac_dc   = sum_ac_dc + excluded.sum_ac_dc,
                    sum_voltage = sum_voltage + excluded.sum_voltage",
            )?;
            for (bin, s) in bins {
                insert.execute(params![
                    day,
                    bin,
                    i64::try_from(s.n)?,
                    s.sum_ac,
                    s.sum_dc,
                    s.sum_ac2,
                    s.sum_dc2,
                    s.sum_ac_dc,
                    s.sum_voltage
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dess_core::record::{Recorder, Sample};
    use jiff::{SignedDuration, Timestamp};

    fn record(start: &str, seconds: i64) -> SlotRecord {
        let start: Timestamp = start.parse().unwrap();
        let mut recorder = Recorder::new(SignedDuration::from_secs(10));
        for i in 0..=seconds {
            recorder.push(Sample {
                at: start + SignedDuration::from_secs(i),
                soc_pct: 50.0,
                battery: Watts(3600.0),
                battery_voltage: 52.0,
                grid: Watts(0.0),
                pv_ac: Watts(0.0),
                pv_dc: Watts(0.0),
                load_out: Watts(0.0),
                load_in: Watts(0.0),
                inverter_ac: Watts(3800.0),
                grid_connected: true,
                relays: [None, None],
                setpoint: None,
            });
        }
        recorder.flush().unwrap()
    }

    #[test]
    fn records_from_before_and_after_a_restart_merge() {
        let store = Store::in_memory().unwrap();
        store
            .save_slot(&record("2026-09-27T11:00:00Z", 100), 0)
            .unwrap();
        store
            .save_slot(&record("2026-09-27T11:05:00Z", 200), 0)
            .unwrap();
        let slot = Slot::containing("2026-09-27T11:00:00Z".parse().unwrap());
        let (covered, charged): (f64, f64) = store
            .conn
            .query_row(
                "SELECT covered_seconds, battery_charge_wh FROM slot_measurements WHERE slot_start = ?1",
                [slot.start_unix()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!((covered - 300.0).abs() < 1e-9);
        assert!((charged - 300.0).abs() < 1e-9);
    }

    #[test]
    fn migrations_run_once() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(usize::try_from(version).unwrap(), MIGRATIONS.len());
        assert!(Store::init(conn).is_ok());
    }

    #[test]
    fn prices_round_trip_and_count_finals() {
        let mut store = Store::in_memory().unwrap();
        let first = Slot::containing("2026-09-27T12:00:00Z".parse().unwrap());
        let second = first.next();
        store
            .save_prices(&[(first, 100.0)], false, "test", 0)
            .unwrap();
        store
            .save_prices(&[(first, 110.0), (second, 90.0)], true, "test", 1)
            .unwrap();
        let prices = store.prices(first, second.next()).unwrap();
        assert_eq!(prices[&first], EurPerKwh(0.11));
        assert_eq!(store.final_price_count(first, second.next()).unwrap(), 2);
    }

    #[test]
    fn pruning_keeps_the_decided_slot() {
        let store = Store::in_memory().unwrap();
        for (planned_at, slot_start) in [(900, 900), (900, 1800), (1800, 1800)] {
            store
                .conn
                .execute(
                    "INSERT INTO plans VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, 1, 50, 0)",
                    [planned_at, slot_start],
                )
                .unwrap();
        }
        assert_eq!(store.prune_plans(1800).unwrap(), 1);
        let left: i64 = store
            .conn
            .query_row("SELECT count(*) FROM plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }

    #[test]
    fn forecasts_never_overwrite_the_past() {
        let mut store = Store::in_memory().unwrap();
        let now: jiff::Timestamp = "2026-09-27T12:05:00Z".parse().unwrap();
        let past = Slot::containing(now - jiff::SignedDuration::from_mins(30));
        let future = Slot::containing(now).next();
        let sunny = Weather {
            ghi: 800.0,
            dni: 700.0,
            dhi: 100.0,
            temperature: 20.0,
            humidity: 50.0,
            wind: 2.0,
        };
        let cloudy = Weather {
            ghi: 100.0,
            dni: 0.0,
            dhi: 100.0,
            ..sunny
        };
        store
            .save_weather(&BTreeMap::from([(past, sunny)]), true, now)
            .unwrap();
        store
            .save_weather(
                &BTreeMap::from([(past, cloudy), (future, cloudy)]),
                false,
                now,
            )
            .unwrap();
        let stored = store.weather(past, future.next()).unwrap();
        assert_eq!(stored[&past], sunny, "the archive keeps the past");
        assert_eq!(stored[&future], cloudy);
        assert_eq!(store.last_weather_history().unwrap(), Some(past));
    }
}
