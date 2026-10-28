//! The debug download: everything dess-oxide knows, in one file, for
//! looking into how it's doing without screenshots and exports.
//!
//! A copy of the database (measurements, every plan with its forecasts,
//! prices, weather, the learned models), with a `debug` table added: the
//! version, the config without its secrets and location, the control and
//! service status, the current plan, the GX device's published values and
//! the recent log. Gzipped SQLite: open it with `sqlite3` after `gunzip`.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use flate2::Compression;
use flate2::write::GzEncoder;
use jiff::Timestamp;
use rusqlite::Connection;

use crate::run::Shared;

/// The file's name and its gzipped bytes.
pub fn export(shared: &Shared, now: Timestamp) -> anyhow::Result<(String, Vec<u8>)> {
    let copy = std::env::temp_dir().join(format!("dess-oxide-debug-{}.db", now.as_second()));
    let result = write_copy(shared, &copy, now).and_then(|()| gzip(&copy));
    let _ = std::fs::remove_file(&copy);
    let local = now.to_zoned(shared.tz.clone());
    let name = format!("dess-oxide-debug-{}.db.gz", local.strftime("%Y%m%d-%H%M"));
    Ok((name, result?))
}

fn write_copy(shared: &Shared, copy: &Path, now: Timestamp) -> anyhow::Result<()> {
    // A connection of its own: the copy is a read, so the service's own
    // writes carry on meanwhile (WAL).
    let source = Connection::open(shared.data_dir.join("dess.db"))?;
    source
        .execute("VACUUM INTO ?1", [copy.to_string_lossy()])
        .context("copying the database")?;
    drop(source);
    let mut conn = Connection::open(copy)?;
    let tx = conn.transaction()?;
    tx.execute_batch("CREATE TABLE debug (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;")?;
    {
        let mut insert = tx.prepare("INSERT INTO debug VALUES (?1, ?2)")?;
        for (key, value) in facts(shared, now) {
            insert.execute([key, &value])?;
        }
    }
    tx.commit()?;
    conn.execute_batch("VACUUM")?;
    Ok(())
}

fn lock<T: std::fmt::Debug>(value: &std::sync::Mutex<T>) -> String {
    format!("{:#?}", *value.lock().expect("lock poisoned"))
}

fn facts(shared: &Shared, now: Timestamp) -> Vec<(&'static str, String)> {
    let mut facts = vec![
        ("version", env!("CARGO_PKG_VERSION").to_owned()),
        ("generated_at", now.to_string()),
        ("time_zone", shared.tz.iana_name().unwrap_or("?").to_owned()),
        ("config", format!("{:#?}", shared.config.redacted())),
        ("control", format!("{:#?}", shared.control_status())),
        ("status", lock(&shared.status)),
        ("comparison", lock(&shared.comparison)),
        ("soc_estimate", lock(&shared.soc)),
        ("station", lock(&shared.station)),
        ("weather_correction", lock(&shared.weather_correction)),
        ("next_legionella", lock(&shared.next_legionella)),
        ("pv_switch", lock(&shared.pv_switch)),
        ("log", crate::logbuf::recent().join("\n")),
    ];
    if let Some(view) = shared.plan.borrow().clone() {
        facts.extend([
            (
                "plan",
                serde_json::to_string_pretty(&crate::web::plan_value(&view)).unwrap_or_default(),
            ),
            ("battery_model", format!("{:#?}", view.battery)),
            ("power_limits", format!("{:#?}", view.limits)),
            ("planner_settings", format!("{:#?}", view.settings)),
            ("min_soc", view.min_soc.to_string()),
        ]);
    }
    if let Some(venus) = shared.venus.get().and_then(std::sync::Weak::upgrade) {
        let values = venus.with_snapshot(|s| {
            s.with_prefix("")
                .into_iter()
                .map(|(key, entry)| {
                    (
                        key.to_owned(),
                        serde_json::json!({
                            "value": entry.value,
                            "received": entry.received,
                            "changes": entry.changes,
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        });
        facts.push((
            "gx_values",
            serde_json::to_string_pretty(&values).unwrap_or_default(),
        ));
    }
    facts
}

fn gzip(path: &Path) -> anyhow::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 3), Compression::fast());
    encoder.write_all(&bytes)?;
    Ok(encoder.finish()?)
}
