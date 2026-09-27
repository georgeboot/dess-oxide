//! `SQLite` storage in the app's `/data` directory.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use dess_core::efficiency::BinStats;
use dess_core::record::SlotRecord;
use rusqlite::{Connection, params};

/// Schema migrations, applied in order; `PRAGMA user_version` counts how many ran.
const MIGRATIONS: &[&str] = &[r"
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
"];

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    fn in_memory() -> anyhow::Result<Self> {
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
    use dess_core::{Slot, Watts};
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
}
