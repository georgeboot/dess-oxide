//! A read-only report on a GX device's configuration: what dess-oxide will
//! work with, and what has to change before it can take control.

use std::fmt;

use jiff::Timestamp;
use serde::Serialize;

use crate::reading;
use crate::snapshot::Snapshot;

/// The setpoint override (`hub4/0/Overrides/Setpoint`) arrived in Venus OS 3.50.
const MIN_FIRMWARE: (u32, u32) = (3, 50);

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub portal_id: String,
    pub observed_seconds: f64,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub ess: Ess,
    pub battery: Option<Battery>,
    pub inverter: Option<Inverter>,
    pub grid_meters: Vec<Meter>,
    pub pv_inverters: Vec<Meter>,
    pub relays: Vec<Relay>,
    pub dynamic_ess: DynamicEss,
    pub modbus_tcp_enabled: Option<bool>,
    pub flows: Result<Flows, String>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
pub struct Ess {
    pub hub4_mode: Option<u32>,
    pub battery_life_state: Option<u32>,
    pub minimum_soc: Option<f64>,
    pub active_soc_limit: Option<f64>,
    pub runs_without_grid_meter: bool,
    pub has_ac_in_loads: Option<bool>,
    pub setpoint_setting: Option<f64>,
    pub setpoint_setting_changes: u32,
    pub setpoint_override: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Battery {
    pub instance: u32,
    pub product: Option<String>,
    pub installed_capacity_ah: Option<f64>,
    pub soc: Option<f64>,
    pub voltage: Option<f64>,
    pub max_charge_current: Option<f64>,
    pub max_discharge_current: Option<f64>,
    pub max_charge_voltage: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Inverter {
    pub instance: u32,
    pub product: Option<String>,
    pub units: usize,
    pub phases: Option<f64>,
    pub state: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Meter {
    pub instance: u32,
    pub product: Option<String>,
    pub connection: Option<String>,
    pub position: Option<String>,
    pub power: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Relay {
    /// 1-based, as labelled on the device.
    pub number: u32,
    pub function: Option<String>,
    pub inverted: Option<bool>,
    pub closed: Option<bool>,
    pub closed_at_boot: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct DynamicEss {
    pub mode: Option<u32>,
    pub active: Option<bool>,
    pub scheduled_slots: usize,
    pub future_slots: usize,
}

#[derive(Debug, Serialize)]
pub struct Flows {
    pub soc: f64,
    pub battery_w: f64,
    pub grid_w: f64,
    pub pv_w: f64,
    pub load_out_w: f64,
    pub load_in_w: f64,
    pub inverter_ac_w: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    /// Must be fixed before dess-oxide may take control.
    Blocker,
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl ProbeReport {
    pub fn from_snapshot(
        portal_id: &str,
        snapshot: &Snapshot,
        observed_seconds: f64,
        now: Timestamp,
    ) -> Self {
        let s = snapshot;
        let setting = "settings/0/Settings";
        let ess = Ess {
            hub4_mode: uint(s, &format!("{setting}/CGwacs/Hub4Mode")),
            battery_life_state: uint(s, &format!("{setting}/CGwacs/BatteryLife/State")),
            minimum_soc: s.number(&format!("{setting}/CGwacs/BatteryLife/MinimumSocLimit")),
            active_soc_limit: s.number("system/0/Control/ActiveSocLimit"),
            runs_without_grid_meter: s.number(&format!("{setting}/CGwacs/RunWithoutGridMeter"))
                == Some(1.0),
            has_ac_in_loads: flag(s, &format!("{setting}/SystemSetup/HasAcInLoads")),
            setpoint_setting: s.number(&format!("{setting}/CGwacs/AcPowerSetPoint")),
            setpoint_setting_changes: s.changes(&format!("{setting}/CGwacs/AcPowerSetPoint")),
            setpoint_override: s.number("hub4/0/Overrides/Setpoint"),
        };

        let mut report = Self {
            portal_id: portal_id.to_owned(),
            observed_seconds,
            model: text(s, "platform/0/Device/Model"),
            firmware: text(s, "platform/0/Firmware/Installed/Version"),
            ess,
            battery: battery(s),
            inverter: inverter(s),
            grid_meters: meters(s, "grid"),
            pv_inverters: meters(s, "pvinverter"),
            relays: relays(s),
            dynamic_ess: dynamic_ess(s, now),
            modbus_tcp_enabled: flag(s, &format!("{setting}/Services/Modbus")),
            flows: reading::sample(s, now)
                .map(|x| Flows {
                    soc: x.soc_pct,
                    battery_w: x.battery.0,
                    grid_w: x.grid.0,
                    pv_w: x.pv_ac.0 + x.pv_dc.0,
                    load_out_w: x.load_out.0,
                    load_in_w: x.load_in.0,
                    inverter_ac_w: x.inverter_ac.0,
                })
                .map_err(|e| e.to_string()),
            findings: Vec::new(),
        };
        report.findings = findings(&report);
        report
    }

    /// Whether anything blocks dess-oxide from taking control.
    pub fn has_blockers(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Blocker)
    }
}

fn findings(r: &ProbeReport) -> Vec<Finding> {
    let mut out = Vec::new();
    control_findings(r, &mut |severity, message| {
        out.push(Finding { severity, message });
    });
    setup_findings(r, &mut |severity, message| {
        out.push(Finding { severity, message });
    });
    out.sort_by_key(|finding| std::cmp::Reverse(finding.severity));
    out
}

/// Settings that decide whether dess-oxide can control the system.
fn control_findings(r: &ProbeReport, add: &mut impl FnMut(Severity, String)) {
    match r.firmware.as_deref().and_then(parse_version) {
        Some(version) if version < MIN_FIRMWARE => add(
            Severity::Blocker,
            format!(
                "Venus OS {} is older than {}.{}, which lacks the volatile setpoint override.",
                r.firmware.as_deref().unwrap_or("?"),
                MIN_FIRMWARE.0,
                MIN_FIRMWARE.1
            ),
        ),
        Some(_) => {}
        None => add(
            Severity::Warning,
            "Couldn't read the Venus OS version.".into(),
        ),
    }

    match r.ess.hub4_mode {
        Some(1) => {}
        Some(3) => add(
            Severity::Blocker,
            "ESS is in external control mode (Hub4Mode 3); dess-oxide needs ESS to regulate the grid setpoint.".into(),
        ),
        Some(mode) => add(
            Severity::Warning,
            format!("ESS multiphase regulation is Hub4Mode {mode}; dess-oxide expects \"Total of all phases\" (1)."),
        ),
        None => add(Severity::Blocker, "No ESS settings found. Is the ESS assistant installed?".into()),
    }

    match r.ess.battery_life_state {
        Some(1..=8) => add(
            Severity::Info,
            "BatteryLife is on, so the minimum SoC moves by itself. \"Optimized without BatteryLife\" is recommended."
                .into(),
        ),
        Some(9) => add(
            Severity::Warning,
            "ESS is in \"Keep batteries charged\"; the battery won't be used until that's changed.".into(),
        ),
        _ => {}
    }

    match (r.dynamic_ess.mode, r.dynamic_ess.future_slots) {
        (Some(mode @ 1..), _) => add(
            Severity::Blocker,
            format!(
                "Victron Dynamic ESS is enabled (mode {mode}). It writes the same setpoint override, so switch it \
                 off before dess-oxide takes control."
            ),
        ),
        (_, future @ 1..) => add(
            Severity::Warning,
            format!(
                "{future} Dynamic ESS schedule slots lie in the future and would run if DESS were switched on."
            ),
        ),
        _ => {}
    }
}

/// How the system is measured and wired.
fn setup_findings(r: &ProbeReport, add: &mut impl FnMut(Severity, String)) {
    if r.ess.setpoint_setting_changes >= 2 {
        add(
            Severity::Warning,
            format!(
                "The persisted ESS setpoint (Settings/CGwacs/AcPowerSetPoint) changed {} times in {:.0} s. It is \
                 stored on flash; frequent writers should use hub4/0/Overrides/Setpoint instead.",
                r.ess.setpoint_setting_changes, r.observed_seconds
            ),
        );
    }

    if r.ess.runs_without_grid_meter {
        add(
            Severity::Info,
            "No grid meter: the inverters' AC input is the grid measurement, so every load must be on AC-out.".into(),
        );
    } else if r.ess.has_ac_in_loads == Some(false) {
        add(
            Severity::Info,
            "\"AC loads on input\" is off, so Venus doesn't publish them; dess-oxide derives them as grid minus \
             inverter AC-in."
                .into(),
        );
    }

    if let Some(soc) = r.battery.as_ref().and_then(|b| b.soc)
        && soc.fract() == 0.0
    {
        add(
            Severity::Info,
            "The BMS reports SoC in whole percent (at least right now); dess-oxide refines it from battery power."
                .into(),
        );
    }

    let manual: Vec<String> = r
        .relays
        .iter()
        .filter(|relay| relay.function.as_deref() == Some("manual"))
        .map(|relay| relay.number.to_string())
        .collect();
    if manual.is_empty() {
        add(
            Severity::Info,
            "No relay is set to \"Manual\", so none can switch a PV contactor.".into(),
        );
    } else {
        add(
            Severity::Info,
            format!(
                "Relays set to \"Manual\" (usable for a PV contactor): {}.",
                manual.join(", ")
            ),
        );
    }

    if r.pv_inverters.is_empty() {
        add(
            Severity::Info,
            "No AC-coupled PV inverter is visible to Venus.".into(),
        );
    }
    if let Err(error) = &r.flows {
        add(
            Severity::Warning,
            format!("Couldn't read the power flows: {error}."),
        );
    }
}

fn text(s: &Snapshot, key: &str) -> Option<String> {
    s.text(key).map(str::to_owned)
}

fn uint(s: &Snapshot, key: &str) -> Option<u32> {
    s.number(key).map(|v| v as u32)
}

fn flag(s: &Snapshot, key: &str) -> Option<bool> {
    s.number(key).map(|v| v != 0.0)
}

/// `"v3.66"` or `"v3.70~22"` → `(3, 66)` / `(3, 70)`.
fn parse_version(firmware: &str) -> Option<(u32, u32)> {
    let digits = firmware.trim_start_matches('v');
    let (major, rest) = digits.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn battery(s: &Snapshot) -> Option<Battery> {
    // "com.victronenergy.battery/512"
    let instance = s
        .text("system/0/ActiveBatteryService")?
        .rsplit_once('/')?
        .1
        .parse()
        .ok()?;
    let key = |path: &str| format!("battery/{instance}/{path}");
    Some(Battery {
        instance,
        product: text(s, &key("ProductName")),
        installed_capacity_ah: s.number(&key("InstalledCapacity")),
        soc: s.number(&key("Soc")),
        voltage: s.number(&key("Dc/0/Voltage")),
        max_charge_current: s.number(&key("Info/MaxChargeCurrent")),
        max_discharge_current: s.number(&key("Info/MaxDischargeCurrent")),
        max_charge_voltage: s.number(&key("Info/MaxChargeVoltage")),
    })
}

fn inverter(s: &Snapshot) -> Option<Inverter> {
    let instance = uint(s, "system/0/VebusInstance")?;
    let key = |path: &str| format!("vebus/{instance}/{path}");
    let units = (0..32)
        .take_while(|n| s.get(&key(&format!("Devices/{n}/ProductId"))).is_some())
        .count();
    Some(Inverter {
        instance,
        product: text(s, &key("ProductName")),
        units,
        phases: s.number(&key("Ac/NumberOfPhases")),
        state: uint(s, &key("State")).map(vebus_state),
    })
}

fn vebus_state(code: u32) -> String {
    let name = match code {
        0 => "off",
        1 => "low power",
        2 => "fault",
        3 => "bulk",
        4 => "absorption",
        5 => "float",
        6 => "storage",
        7 => "equalize",
        8 => "passthru",
        9 => "inverting",
        10 => "power assist",
        11 => "power supply",
        244 => "sustain",
        252 => "external control",
        other => return format!("state {other}"),
    };
    name.to_owned()
}

fn meters(s: &Snapshot, service: &str) -> Vec<Meter> {
    s.instances(service)
        .into_iter()
        .map(|instance| {
            let key = |path: &str| format!("{service}/{instance}/{path}");
            Meter {
                instance,
                product: text(s, &key("ProductName")),
                connection: text(s, &key("Mgmt/Connection")),
                position: uint(s, &key("Position")).map(|p| {
                    match p {
                        0 => "AC-in 1",
                        1 => "AC-out",
                        2 => "AC-in 2",
                        _ => "unknown",
                    }
                    .to_owned()
                }),
                power: s.number(&key("Ac/Power")),
            }
        })
        .collect()
}

fn relays(s: &Snapshot) -> Vec<Relay> {
    (0..2)
        .filter_map(|index: u32| {
            // Relay 1's settings predate multiple relays and have no index.
            let setting = |name: &str| match index {
                0 => format!("settings/0/Settings/Relay/{name}"),
                n => format!("settings/0/Settings/Relay/{n}/{name}"),
            };
            let closed = flag(s, &format!("system/0/Relay/{index}/State"));
            let function = uint(s, &setting("Function"));
            (closed.is_some() || function.is_some()).then(|| Relay {
                number: index + 1,
                function: function.map(relay_function),
                inverted: flag(s, &setting("Polarity")),
                closed,
                closed_at_boot: flag(
                    s,
                    &format!("settings/0/Settings/Relay/{index}/InitialState"),
                ),
            })
        })
        .collect()
}

fn relay_function(code: u32) -> String {
    let name = match code {
        0 => "alarm",
        1 => "genset",
        2 => "manual",
        3 => "tank pump",
        4 => "temperature",
        5 => "genset helper",
        6 => "opportunity loads",
        other => return format!("function {other}"),
    };
    name.to_owned()
}

fn dynamic_ess(s: &Snapshot, now: Timestamp) -> DynamicEss {
    let now = now.as_second() as f64;
    let mut scheduled = 0;
    let mut future = 0;
    for slot in 0..48 {
        let key = |field: &str| format!("settings/0/Settings/DynamicEss/Schedule/{slot}/{field}");
        let Some(start) = s.number(&key("Start")).filter(|start| *start > 0.0) else {
            continue;
        };
        scheduled += 1;
        if start + s.number(&key("Duration")).unwrap_or(0.0) > now {
            future += 1;
        }
    }
    DynamicEss {
        mode: uint(s, "settings/0/Settings/DynamicEss/Mode"),
        active: flag(s, "system/0/DynamicEss/Active"),
        scheduled_slots: scheduled,
        future_slots: future,
    }
}

fn opt<T: fmt::Display>(value: Option<&T>) -> String {
    value.map_or_else(|| "–".to_owned(), ToString::to_string)
}

impl fmt::Display for ProbeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} {} (portal {}), observed for {:.0} s",
            opt(self.model.as_ref()),
            opt(self.firmware.as_ref()),
            self.portal_id,
            self.observed_seconds
        )?;
        self.fmt_ess(f)?;
        self.fmt_devices(f)?;
        self.fmt_status(f)
    }
}

impl ProbeReport {
    fn fmt_ess(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nESS")?;
        let e = &self.ess;
        writeln!(f, "  Hub4Mode                {}", opt(e.hub4_mode.as_ref()))?;
        writeln!(
            f,
            "  BatteryLife state       {}",
            opt(e.battery_life_state.as_ref())
        )?;
        writeln!(
            f,
            "  Minimum SoC             {} % (active limit {} %)",
            opt(e.minimum_soc.as_ref()),
            opt(e.active_soc_limit.as_ref())
        )?;
        writeln!(
            f,
            "  Grid metering           {}",
            if e.runs_without_grid_meter {
                "inverter AC-in (no grid meter)"
            } else {
                "grid meter"
            }
        )?;
        writeln!(
            f,
            "  Setpoint setting        {} W ({} changes)",
            opt(e.setpoint_setting.as_ref()),
            e.setpoint_setting_changes
        )?;
        writeln!(
            f,
            "  Setpoint override       {}",
            opt(e.setpoint_override.as_ref())
        )
    }

    fn fmt_devices(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nDevices")?;
        if let Some(i) = &self.inverter {
            writeln!(
                f,
                "  Inverter/charger        {} × {} ({} phases, {})",
                i.units,
                opt(i.product.as_ref()),
                opt(i.phases.as_ref()),
                opt(i.state.as_ref())
            )?;
        }
        if let Some(b) = &self.battery {
            writeln!(
                f,
                "  Battery                 {} #{}: {} Ah, SoC {} %, {} V, CCL {} A, DCL {} A, CVL {} V",
                opt(b.product.as_ref()),
                b.instance,
                opt(b.installed_capacity_ah.as_ref()),
                opt(b.soc.as_ref()),
                opt(b.voltage.map(|v| format!("{v:.2}")).as_ref()),
                opt(b.max_charge_current.as_ref()),
                opt(b.max_discharge_current.as_ref()),
                opt(b.max_charge_voltage.map(|v| format!("{v:.1}")).as_ref()),
            )?;
        }
        for (label, list) in [
            ("Grid meter", &self.grid_meters),
            ("PV inverter", &self.pv_inverters),
        ] {
            for m in list {
                writeln!(
                    f,
                    "  {label:<24}{} #{}{} via {}, {} W",
                    opt(m.product.as_ref()),
                    m.instance,
                    m.position
                        .as_ref()
                        .map_or_else(String::new, |p| format!(" on {p}")),
                    opt(m.connection.as_ref()),
                    opt(m.power.as_ref())
                )?;
            }
        }
        for r in &self.relays {
            writeln!(
                f,
                "  Relay {}                 {}{}, {}, boots {}",
                r.number,
                opt(r.function.as_ref()),
                if r.inverted == Some(true) {
                    " (inverted)"
                } else {
                    ""
                },
                match r.closed {
                    Some(true) => "closed",
                    Some(false) => "open",
                    None => "state unknown",
                },
                match r.closed_at_boot {
                    Some(true) => "closed",
                    Some(false) => "open",
                    None => "–",
                },
            )?;
        }
        let d = &self.dynamic_ess;
        writeln!(
            f,
            "  Dynamic ESS             mode {}, {} scheduled slots ({} in the future)",
            opt(d.mode.as_ref()),
            d.scheduled_slots,
            d.future_slots
        )?;
        writeln!(
            f,
            "  Modbus TCP server       {}",
            match self.modbus_tcp_enabled {
                Some(true) => "enabled",
                Some(false) => "disabled",
                None => "–",
            }
        )
    }

    fn fmt_status(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nPower flows now")?;
        match &self.flows {
            Ok(x) => writeln!(
                f,
                "  SoC {:.1} %  battery {:+.0} W  grid {:+.0} W  PV {:.0} W  loads {:.0} W (+{:.0} W before the inverters)",
                x.soc, x.battery_w, x.grid_w, x.pv_w, x.load_out_w, x.load_in_w
            )?,
            Err(error) => writeln!(f, "  unavailable: {error}")?,
        }

        writeln!(f, "\nFindings")?;
        for finding in &self.findings {
            let tag = match finding.severity {
                Severity::Blocker => "BLOCKER",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            writeln!(f, "  [{tag}] {}", finding.message)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    #[test]
    fn parses_firmware_versions() {
        assert_eq!(parse_version("v3.66"), Some((3, 66)));
        assert_eq!(parse_version("v3.70~22"), Some((3, 70)));
        assert_eq!(parse_version("garbage"), None);
    }

    fn snapshot(values: &[(&str, Value)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (key, value) in values {
            s.update(key, value.clone(), Timestamp::UNIX_EPOCH);
        }
        s
    }

    #[test]
    fn active_dynamic_ess_blocks_control() {
        let s = snapshot(&[
            (
                "platform/0/Firmware/Installed/Version",
                Value::Text("v3.75".into()),
            ),
            ("settings/0/Settings/CGwacs/Hub4Mode", Value::Number(1.0)),
            ("settings/0/Settings/DynamicEss/Mode", Value::Number(1.0)),
        ]);
        let report = ProbeReport::from_snapshot("abc", &s, 10.0, Timestamp::UNIX_EPOCH);
        assert!(report.has_blockers());
        assert!(
            report.findings[0]
                .message
                .contains("Dynamic ESS is enabled")
        );
    }

    #[test]
    fn frequent_setpoint_writes_are_flagged() {
        let mut s = snapshot(&[
            (
                "platform/0/Firmware/Installed/Version",
                Value::Text("v3.66".into()),
            ),
            ("settings/0/Settings/CGwacs/Hub4Mode", Value::Number(1.0)),
            ("settings/0/Settings/DynamicEss/Mode", Value::Number(0.0)),
        ]);
        for w in [100.0, 200.0, 300.0] {
            s.update(
                "settings/0/Settings/CGwacs/AcPowerSetPoint",
                Value::Number(w),
                Timestamp::UNIX_EPOCH,
            );
        }
        let report = ProbeReport::from_snapshot("abc", &s, 25.0, Timestamp::UNIX_EPOCH);
        assert!(!report.has_blockers());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("changed 2 times"))
        );
    }

    #[test]
    fn old_dynamic_ess_slots_are_harmless() {
        let s = snapshot(&[
            ("settings/0/Settings/DynamicEss/Mode", Value::Number(0.0)),
            (
                "settings/0/Settings/DynamicEss/Schedule/0/Start",
                Value::Number(1_749_315_600.0),
            ),
            (
                "settings/0/Settings/DynamicEss/Schedule/0/Duration",
                Value::Number(900.0),
            ),
        ]);
        let now = Timestamp::from_second(1_790_000_000).unwrap();
        let report = ProbeReport::from_snapshot("abc", &s, 10.0, now);
        assert_eq!(report.dynamic_ess.scheduled_slots, 1);
        assert_eq!(report.dynamic_ess.future_slots, 0);
    }
}
