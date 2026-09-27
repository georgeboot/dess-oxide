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
    // ESS only regulates towards a grid setpoint in modes 1 and 2; in
    // external control (3) the stored setpoint means nothing.
    let regulating = matches!(
        snapshot.number("settings/0/Settings/CGwacs/Hub4Mode"),
        Some(mode) if mode == 1.0 || mode == 2.0
    );
    let setpoint = snapshot
        .number("hub4/0/Overrides/Setpoint")
        .or_else(|| snapshot.number("settings/0/Settings/CGwacs/AcPowerSetPoint"))
        .filter(|_| regulating)
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

/// What the planner needs to know about the battery and inverters.
#[derive(Debug, Clone, PartialEq)]
pub struct BatteryInfo {
    /// Usable capacity: Venus's Dynamic ESS capacity setting, or the BMS's
    /// installed Ah at nominal LFP voltage.
    pub capacity_wh: Option<f64>,
    /// Number of inverter/charger units on the VE.Bus.
    pub inverter_units: u32,
    /// BMS charge and discharge current limits (CCL/DCL), A.
    pub max_charge_current: Option<f64>,
    pub max_discharge_current: Option<f64>,
    pub voltage: Option<f64>,
    /// The minimum SoC ESS enforces right now, %.
    pub active_min_soc: Option<f64>,
}

pub fn battery_info(snapshot: &Snapshot) -> BatteryInfo {
    // "com.victronenergy.battery/512"
    let instance = snapshot
        .text("system/0/ActiveBatteryService")
        .and_then(|service| service.rsplit_once('/'))
        .map(|(_, instance)| instance.to_owned());
    let battery = |path: &str| {
        instance
            .as_ref()
            .and_then(|i| snapshot.number(&format!("battery/{i}/{path}")))
    };
    let from_bms = || {
        let ah = battery("InstalledCapacity")?;
        // LFP: about 3.5 V per cell at the charge voltage, 3.2 V nominal.
        let cells = (battery("Info/MaxChargeVoltage")? / 3.5).round();
        Some(ah * cells * 3.2)
    };
    let capacity_wh = snapshot
        .number("settings/0/Settings/DynamicEss/BatteryCapacity")
        .filter(|kwh| *kwh > 0.0)
        .map(|kwh| kwh * 1000.0)
        .or_else(from_bms);
    let vebus = snapshot
        .number("system/0/VebusInstance")
        .map(|i| format!("vebus/{i}"));
    let inverter_units = vebus.map_or(1, |vebus| {
        (0..32)
            .take_while(|n| {
                snapshot
                    .get(&format!("{vebus}/Devices/{n}/ProductId"))
                    .is_some()
            })
            .count()
            .max(1) as u32
    });
    BatteryInfo {
        capacity_wh,
        inverter_units,
        max_charge_current: battery("Info/MaxChargeCurrent"),
        max_discharge_current: battery("Info/MaxDischargeCurrent"),
        voltage: snapshot.number("system/0/Dc/Battery/Voltage"),
        active_min_soc: snapshot.number("system/0/Control/ActiveSocLimit"),
    }
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
            ("settings/0/Settings/CGwacs/Hub4Mode", n(1.0)),
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
    fn no_setpoint_in_external_control() {
        // DAO's bypass: ESS mode 3, the stored setpoint left behind.
        let mut values = georges_cerbo();
        values.retain(|(k, _)| !k.ends_with("Hub4Mode"));
        values.push(("settings/0/Settings/CGwacs/Hub4Mode", n(3.0)));
        let s = sample(&snapshot(&values), Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(s.setpoint, None);
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

    #[test]
    fn battery_info_prefers_the_dess_capacity_setting() {
        let mut values = georges_cerbo();
        values.extend([
            (
                "system/0/ActiveBatteryService",
                Value::Text("com.victronenergy.battery/512".into()),
            ),
            ("battery/512/InstalledCapacity", n(628.0)),
            ("battery/512/Info/MaxChargeVoltage", n(56.8)),
            ("battery/512/Info/MaxChargeCurrent", n(247.0)),
            ("vebus/276/Devices/0/ProductId", n(9763.0)),
            ("vebus/276/Devices/1/ProductId", n(9763.0)),
            ("vebus/276/Devices/2/ProductId", n(9763.0)),
            ("system/0/Control/ActiveSocLimit", n(5.0)),
        ]);
        let info = battery_info(&snapshot(&values));
        assert_eq!(info.inverter_units, 3);
        assert_eq!(info.max_charge_current, Some(247.0));
        // 628 Ah × 16 cells × 3.2 V without the DESS setting…
        assert!((info.capacity_wh.unwrap() - 32_153.6).abs() < 1e-6);
        // …and the setting when it's there.
        values.push(("settings/0/Settings/DynamicEss/BatteryCapacity", n(32.0)));
        assert_eq!(battery_info(&snapshot(&values)).capacity_wh, Some(32_000.0));
    }
}
