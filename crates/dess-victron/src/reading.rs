//! Typed readings out of a [`Snapshot`].

use dess_core::Watts;
use dess_core::record::Sample;
use jiff::Timestamp;

use crate::snapshot::Snapshot;

/// Active input source value meaning "not connected" (inverting from battery).
const SOURCE_DISCONNECTED: f64 = 240.0;
/// Anything beyond this is a measurement glitch for a home system.
const MAX_PLAUSIBLE_W: f64 = 100_000.0;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReadingError {
    #[error("{0} is not available")]
    Missing(String),
    #[error("{key} = {value} is implausible")]
    Implausible { key: String, value: f64 },
}

/// Builds a system sample from the current snapshot.
///
/// Venus publishes phase-level `null`s for phases that don't exist and for
/// devices that are off (a PV inverter at night), so those count as 0. The
/// battery, grid and consumption values are required.
pub fn sample(snapshot: &Snapshot, at: Timestamp) -> Result<Sample, ReadingError> {
    let soc_pct = required(snapshot, "system/0/Dc/Battery/Soc")?;
    if !(0.0..=100.0).contains(&soc_pct) {
        return Err(ReadingError::Implausible {
            key: "system/0/Dc/Battery/Soc".into(),
            value: soc_pct,
        });
    }
    let battery = power(snapshot, "system/0/Dc/Battery/Power")?;
    let battery_voltage = required(snapshot, "system/0/Dc/Battery/Voltage")?;

    let grid = phases(snapshot, "system/0/Ac/Grid")?.ok_or_else(|| missing("system/0/Ac/Grid"))?;
    let pv_on_grid = phases(snapshot, "system/0/Ac/PvOnGrid")?.unwrap_or(Watts::ZERO);
    let pv_on_output = phases(snapshot, "system/0/Ac/PvOnOutput")?.unwrap_or(Watts::ZERO);
    let pv_dc = optional_power(snapshot, "system/0/Dc/Pv/Power")?.unwrap_or(Watts::ZERO);
    let load_out = phases(snapshot, "system/0/Ac/ConsumptionOnOutput")?
        .ok_or_else(|| missing("system/0/Ac/ConsumptionOnOutput"))?;

    let vebus = snapshot
        .number("system/0/VebusInstance")
        .ok_or_else(|| missing("system/0/VebusInstance"))?;
    let vebus = format!("vebus/{vebus}");
    let inverter_in = vebus_phases(snapshot, &vebus, "Ac/ActiveIn")?;
    let inverter_out = vebus_phases(snapshot, &vebus, "Ac/Out")?;

    // Without a grid meter the inverters' AC input *is* the grid measurement,
    // so nothing can sit before them. Otherwise Victron only publishes
    // consumption on input when "AC loads on input" is enabled; derive it
    // when it doesn't: whatever the grid delivers that doesn't reach the inverters.
    let without_grid_meter =
        snapshot.number("settings/0/Settings/CGwacs/RunWithoutGridMeter") == Some(1.0);
    let load_in = match phases(snapshot, "system/0/Ac/ConsumptionOnInput")? {
        _ if without_grid_meter => Watts::ZERO,
        Some(load) => load,
        None => grid + pv_on_grid - inverter_in,
    };

    let grid_connected =
        snapshot.number("system/0/Ac/ActiveIn/Source") != Some(SOURCE_DISCONNECTED);
    let relays = [0, 1].map(|i| {
        snapshot
            .number(&format!("system/0/Relay/{i}/State"))
            .map(|s| s != 0.0)
    });
    let setpoint = snapshot
        .number("hub4/0/Overrides/Setpoint")
        .or_else(|| snapshot.number("settings/0/Settings/CGwacs/AcPowerSetPoint"))
        .map(Watts);

    Ok(Sample {
        at,
        soc_pct,
        battery,
        battery_voltage,
        grid,
        pv_ac: pv_on_grid + pv_on_output,
        pv_dc,
        load_out,
        load_in,
        inverter_ac: inverter_in - inverter_out,
        grid_connected,
        relays,
        setpoint,
    })
}

fn missing(key: &str) -> ReadingError {
    ReadingError::Missing(key.to_owned())
}

fn required(snapshot: &Snapshot, key: &str) -> Result<f64, ReadingError> {
    snapshot.number(key).ok_or_else(|| missing(key))
}

fn plausible(key: &str, value: f64) -> Result<Watts, ReadingError> {
    if value.abs() > MAX_PLAUSIBLE_W {
        return Err(ReadingError::Implausible {
            key: key.to_owned(),
            value,
        });
    }
    Ok(Watts(value))
}

fn power(snapshot: &Snapshot, key: &str) -> Result<Watts, ReadingError> {
    plausible(key, required(snapshot, key)?)
}

fn optional_power(snapshot: &Snapshot, key: &str) -> Result<Option<Watts>, ReadingError> {
    snapshot.number(key).map(|v| plausible(key, v)).transpose()
}

/// Sum of `<prefix>/L1..L3/Power`; `None` if no phase has a value.
fn phases(snapshot: &Snapshot, prefix: &str) -> Result<Option<Watts>, ReadingError> {
    sum_phases(snapshot, |phase| format!("{prefix}/L{phase}/Power"))
}

/// Sum of the VE.Bus per-phase power `<vebus>/<prefix>/L1..L3/P`.
fn vebus_phases(snapshot: &Snapshot, vebus: &str, prefix: &str) -> Result<Watts, ReadingError> {
    sum_phases(snapshot, |phase| format!("{vebus}/{prefix}/L{phase}/P"))?
        .ok_or_else(|| missing(&format!("{vebus}/{prefix}")))
}

fn sum_phases(
    snapshot: &Snapshot,
    key: impl Fn(u8) -> String,
) -> Result<Option<Watts>, ReadingError> {
    let mut total: Option<Watts> = None;
    for phase in 1..=3 {
        let key = key(phase);
        if let Some(power) = optional_power(snapshot, &key)? {
            *total.get_or_insert(Watts::ZERO) += power;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn snapshot(values: &[(&str, Value)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (key, value) in values {
            s.update(key, value.clone(), Timestamp::UNIX_EPOCH);
        }
        s
    }

    fn n(v: f64) -> Value {
        Value::Number(v)
    }

    /// Values from George's Cerbo (no grid meter, AC-coupled PV on AC-out).
    fn georges_cerbo() -> Vec<(&'static str, Value)> {
        vec![
            ("system/0/Dc/Battery/Soc", n(28.0)),
            ("system/0/Dc/Battery/Power", n(2038.0)),
            ("system/0/Dc/Battery/Voltage", n(52.96)),
            ("system/0/Ac/Grid/L1/Power", n(2786.0)),
            ("system/0/Ac/Grid/L2/Power", n(-710.0)),
            ("system/0/Ac/Grid/L3/Power", n(-797.0)),
            ("system/0/Ac/PvOnGrid/L1/Power", Value::Null),
            ("system/0/Ac/PvOnOutput/L1/Power", n(1423.2)),
            ("system/0/Ac/PvOnOutput/L2/Power", n(1526.2)),
            ("system/0/Ac/PvOnOutput/L3/Power", n(1511.9)),
            ("system/0/Dc/Pv/Power", Value::Null),
            ("system/0/Ac/ConsumptionOnOutput/L1/Power", n(3499.2)),
            ("system/0/Ac/ConsumptionOnOutput/L2/Power", n(123.2)),
            ("system/0/Ac/ConsumptionOnOutput/L3/Power", n(23.9)),
            ("system/0/Ac/ConsumptionOnInput/L1/Power", Value::Null),
            ("system/0/VebusInstance", n(276.0)),
            ("vebus/276/Ac/ActiveIn/L1/P", n(2786.0)),
            ("vebus/276/Ac/ActiveIn/L2/P", n(-710.0)),
            ("vebus/276/Ac/ActiveIn/L3/P", n(-797.0)),
            ("vebus/276/Ac/Out/L1/P", n(2076.0)),
            ("vebus/276/Ac/Out/L2/P", n(-1403.0)),
            ("vebus/276/Ac/Out/L3/P", n(-1488.0)),
            ("system/0/Ac/ActiveIn/Source", n(1.0)),
            ("system/0/Relay/0/State", n(0.0)),
            ("system/0/Relay/1/State", n(0.0)),
            ("hub4/0/Overrides/Setpoint", Value::Null),
            ("settings/0/Settings/CGwacs/RunWithoutGridMeter", n(1.0)),
            ("settings/0/Settings/CGwacs/AcPowerSetPoint", n(2189.0)),
        ]
    }

    #[test]
    fn reads_georges_cerbo() {
        let s = sample(&snapshot(&georges_cerbo()), Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(s.soc_pct, 28.0);
        assert_eq!(s.grid, Watts(1279.0));
        assert!((s.pv_ac.0 - 4461.3).abs() < 1e-9);
        assert_eq!(s.pv_dc, Watts::ZERO);
        assert_eq!(
            s.load_in,
            Watts::ZERO,
            "no grid meter, so nothing sits before the inverters"
        );
        assert_eq!(s.inverter_ac, Watts(2094.0));
        assert!(s.grid_connected);
        assert_eq!(s.relays, [Some(false), Some(false)]);
        assert_eq!(
            s.setpoint,
            Some(Watts(2189.0)),
            "falls back to the setting without an override"
        );
    }

    #[test]
    fn derives_loads_on_input_from_the_grid_meter() {
        let mut values = georges_cerbo();
        values.retain(|(k, _)| {
            !k.starts_with("system/0/Ac/Grid/") && !k.ends_with("RunWithoutGridMeter")
        });
        // 1279 W reaches the inverters, the other 1500 W is used before them.
        values.push(("system/0/Ac/Grid/L1/Power", n(2779.0)));
        let s = sample(&snapshot(&values), Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(s.load_in, Watts(1500.0));
    }

    #[test]
    fn missing_battery_is_an_error() {
        let mut values = georges_cerbo();
        values.retain(|(k, _)| *k != "system/0/Dc/Battery/Soc");
        assert_eq!(
            sample(&snapshot(&values), Timestamp::UNIX_EPOCH),
            Err(ReadingError::Missing("system/0/Dc/Battery/Soc".into()))
        );
    }

    #[test]
    fn grid_loss_is_detected() {
        let mut values = georges_cerbo();
        values.push(("system/0/Ac/ActiveIn/Source", n(240.0)));
        let s = sample(&snapshot(&values), Timestamp::UNIX_EPOCH).unwrap();
        assert!(!s.grid_connected);
    }
}
