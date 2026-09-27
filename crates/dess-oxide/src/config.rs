//! Configuration: `/data/options.json` in the Home Assistant app, or a TOML
//! file when running standalone. Both use the same schema.
//!
//! Only what can't be measured lives here. Units: kW, kWh, €/kWh, degrees.
//!
//! Unknown options are logged and ignored rather than fatal: Home Assistant
//! and the image can briefly disagree about the schema during an update.

use std::path::Path;

use anyhow::{Context, bail};
use dess_core::EurPerKwh;
use dess_core::tariff::{Schedule, Tariff};
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub victron: VictronConfig,
    /// A dry run never writes to the GX device: it plans and shows only. On
    /// unless set to `false`. Turning it off is the first of two locks; the
    /// second is the switch on the dess-oxide page.
    #[serde(default = "default_true")]
    pub dryrun: bool,
    #[serde(default)]
    pub grid: GridConfig,
    #[serde(default)]
    pub battery: BatteryConfig,
    #[serde(default)]
    pub prices: PricesConfig,
    /// Needed for planning; the recorder runs without it.
    #[serde(default)]
    pub tariff: Option<TariffConfig>,
    /// Defaults to Home Assistant's location when running as an app.
    #[serde(default)]
    pub location: Option<LocationConfig>,
    /// Optional starting point for the PV model; M2 learns the real values.
    #[serde(default)]
    pub pv: Vec<PvArrayConfig>,
    /// HA energy sensors whose long-term statistics bootstrap the history.
    #[serde(default)]
    pub history: HistoryConfig,
    /// Standalone only: where Home Assistant is. The app uses the Supervisor.
    #[serde(default)]
    pub homeassistant: Option<HomeAssistantConfig>,
    /// The flexible run whose cheapest start time is shown (the dishwasher).
    #[serde(default)]
    pub cheapest_start: CheapestStartConfig,
    /// Publishes a few Home Assistant entities for automations. Off by default.
    #[serde(default)]
    pub ha_entities: bool,
    #[serde(default)]
    pub ev: EvConfig,
}

/// An EV charger (PLAN.md §12.6). It isn't forecast: it's left out of the
/// house load, and while it charges the per-second control sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EvConfig {
    /// The charger sits between the grid meter and the inverters, so what
    /// Venus sees as load on the inverter input is the EV.
    pub on_input: bool,
}

/// A flexible run, such as the dishwasher: dess-oxide finds its cheapest
/// start time in a nightly window (PLAN.md §14.4).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct CheapestStartConfig {
    /// How long the run takes.
    pub hours: f64,
    /// What it uses.
    pub kwh: f64,
    /// Local time it may start from, `HH:MM`.
    pub earliest: Time,
    /// Local time it must be done by, `HH:MM`: the next day if not after
    /// `earliest`.
    pub finish_by: Time,
}

impl Default for CheapestStartConfig {
    fn default() -> Self {
        Self {
            hours: 3.0,
            kwh: 1.0,
            earliest: Time::constant(20, 0, 0, 0),
            finish_by: Time::constant(8, 0, 0, 0),
        }
    }
}

/// Energy sensors (cumulative kWh, as in HA's energy dashboard). House load
/// is derived as `grid_import − grid_export + pv − battery_in + battery_out`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub grid_import: Option<String>,
    pub grid_export: Option<String>,
    pub pv: Option<String>,
    /// AC energy into the battery system.
    pub battery_in: Option<String>,
    /// AC energy out of the battery system.
    pub battery_out: Option<String>,
    pub heat_pump: Option<String>,
    /// The EV charger, left out of the house load.
    pub ev: Option<String>,
}

impl VictronConfig {
    /// The PV relay's index (0 or 1) and whether closed means PV on.
    pub fn pv_relay_state(&self) -> Option<(usize, bool)> {
        self.pv_relay.map(|relay| {
            let closed_means_on = self.pv_relay_energized == RelayAction::PvOn;
            (usize::from(relay.clamp(1, 2) - 1), closed_means_on)
        })
    }
}

impl HistoryConfig {
    /// `(role, entity)` for every configured sensor.
    pub fn entities(&self) -> Vec<(&'static str, &str)> {
        [
            ("grid_import", &self.grid_import),
            ("grid_export", &self.grid_export),
            ("pv", &self.pv),
            ("battery_in", &self.battery_in),
            ("battery_out", &self.battery_out),
            ("heat_pump", &self.heat_pump),
            ("ev", &self.ev),
        ]
        .into_iter()
        .filter_map(|(role, entity)| {
            entity
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|e| (role, e))
        })
        .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HomeAssistantConfig {
    /// e.g. `http://homeassistant.local:8123`
    pub url: String,
    /// A long-lived access token.
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VictronConfig {
    /// Host name or IP address of the GX device.
    pub host: String,
    /// Port of the GX device's MQTT broker.
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    /// VRM portal id; discovered when not set.
    #[serde(default)]
    pub portal_id: Option<String>,
    /// Cerbo relay (1 or 2) that drives a PV contactor, if any.
    #[serde(default)]
    pub pv_relay: Option<u8>,
    /// What an energised (closed) relay does to the PV.
    #[serde(default)]
    pub pv_relay_energized: RelayAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayAction {
    /// Energising the relay disconnects the PV (the fail-safe wiring).
    #[default]
    PvOff,
    PvOn,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct GridConfig {
    pub max_import_kw: f64,
    pub max_export_kw: f64,
}

impl Default for GridConfig {
    fn default() -> Self {
        // 3 × 25 A, the usual Dutch three-phase connection.
        Self {
            max_import_kw: 17.0,
            max_export_kw: 17.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BatteryConfig {
    /// Usable capacity. Defaults to Venus's Dynamic ESS capacity setting, or
    /// the BMS's installed Ah at nominal LFP voltage.
    pub capacity_kwh: Option<f64>,
    /// Cost per kWh moved in or out of the battery.
    pub wear_cost_eur_per_kwh: f64,
    /// Always keep at least this much for outages, on top of ESS's minimum SoC.
    pub reserve_soc: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct PricesConfig {
    /// Nord Pool delivery area.
    pub area: String,
}

impl Default for PricesConfig {
    fn default() -> Self {
        Self {
            area: "NL".to_owned(),
        }
    }
}

/// A value that takes effect on a date.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Change {
    pub from: Date,
    pub value: f64,
}

/// €/kWh excluding VAT; see [`Tariff`] for the formulas.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TariffConfig {
    pub vat: Vec<Change>,
    pub energy_tax: Vec<Change>,
    pub markup_buy: Vec<Change>,
    pub markup_sell: Vec<Change>,
    /// Last day exports are netted against imports (salderen ends 2027-01-01).
    #[serde(default)]
    pub net_metering_until: Option<Date>,
    /// Whether exports exceed imports over the netting period; then the
    /// energy tax isn't at stake on the marginal kWh.
    #[serde(default)]
    pub net_exporter: bool,
    /// Whether the supplier pays VAT on exports after net metering ends.
    #[serde(default)]
    pub vat_on_export: bool,
    #[serde(default = "default_time_zone")]
    pub time_zone: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct LocationConfig {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct PvArrayConfig {
    pub kwp: f64,
    /// Degrees from horizontal.
    pub tilt: f64,
    /// Compass degrees: 90 = east, 180 = south, 270 = west.
    pub azimuth: f64,
}

fn default_true() -> bool {
    true
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_time_zone() -> String {
    "Europe/Amsterdam".to_owned()
}

impl Config {
    /// Whether dess-oxide may write to the GX device at all (`dryrun: false`).
    pub fn writes_allowed(&self) -> bool {
        !self.dryrun
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let is_json = path.extension().is_some_and(|ext| ext == "json");
        let (config, ignored) =
            Self::parse(&text, is_json).with_context(|| format!("parsing {}", path.display()))?;
        for option in ignored {
            tracing::warn!(%option, "ignoring an option this version doesn't know");
        }
        config.validate()?;
        Ok(config)
    }

    /// Parses JSON or TOML, and returns the paths of options it didn't know.
    fn parse(text: &str, is_json: bool) -> anyhow::Result<(Self, Vec<String>)> {
        let mut ignored = Vec::new();
        let config = if is_json {
            let value: serde_json::Value = serde_json::from_str(text)?;
            serde_ignored::deserialize(value, |path| ignored.push(path.to_string()))?
        } else {
            let value: toml::Value = toml::from_str(text)?;
            serde_ignored::deserialize(value, |path| ignored.push(path.to_string()))?
        };
        Ok((config, ignored))
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.victron.host.trim().is_empty() {
            bail!("victron.host is empty; set it to the GX device's address");
        }
        if self
            .victron
            .pv_relay
            .is_some_and(|relay| !(1..=2).contains(&relay))
        {
            bail!("victron.pv_relay must be 1 or 2");
        }
        if !(0.0..=100.0).contains(&self.battery.reserve_soc) {
            bail!("battery.reserve_soc must be between 0 and 100");
        }
        for (i, array) in self.pv.iter().enumerate() {
            if array.kwp <= 0.0
                || !(0.0..=90.0).contains(&array.tilt)
                || !(0.0..=360.0).contains(&array.azimuth)
            {
                bail!("pv[{i}]: kwp must be positive, tilt 0–90 and azimuth 0–360");
            }
        }
        let run = &self.cheapest_start;
        if !(0.25..=24.0).contains(&run.hours) || run.kwh <= 0.0 {
            bail!("cheapest_start: hours must be 0.25–24 and kwh positive");
        }
        if let Some(tariff) = &self.tariff {
            tariff.to_tariff()?;
        }
        Ok(())
    }
}

impl TariffConfig {
    pub fn to_tariff(&self) -> anyhow::Result<Tariff> {
        let schedule = |component: &'static str, changes: &[Change]| {
            Schedule::new(component, changes.iter().map(|c| (c.from, c.value)))
        };
        let prices = |component: &'static str, changes: &[Change]| {
            Schedule::new(
                component,
                changes.iter().map(|c| (c.from, EurPerKwh(c.value))),
            )
        };
        Ok(Tariff {
            vat: schedule("tariff.vat", &self.vat)?,
            energy_tax: prices("tariff.energy_tax", &self.energy_tax)?,
            markup_buy: prices("tariff.markup_buy", &self.markup_buy)?,
            markup_sell: prices("tariff.markup_sell", &self.markup_sell)?,
            net_metering_until: self.net_metering_until,
            net_exporter: self.net_exporter,
            vat_on_export: self.vat_on_export,
            time_zone: TimeZone::get(&self.time_zone)
                .with_context(|| format!("unknown time zone {}", self.time_zone))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_with_defaults() {
        let config: Config = toml::from_str("[victron]\nhost = \"192.168.1.20\"\n").unwrap();
        assert_eq!(config.victron.port, 1883);
        assert_eq!(config.victron.portal_id, None);
        assert_eq!(config.grid.max_import_kw, 17.0);
        assert_eq!(config.prices.area, "NL");
        assert!(config.tariff.is_none());
    }

    /// A valid value for every option in an app schema.
    fn sample(schema: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match schema {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), sample(v))).collect(),
            Value::Array(items) => items.iter().map(sample).collect(),
            Value::String(kind) => {
                let kind = kind.trim_end_matches('?');
                match kind {
                    "bool" => false.into(),
                    "str" => "192.168.1.20".into(),
                    "port" => 1883.into(),
                    _ if kind.starts_with("float") => 1.0.into(),
                    _ if kind.starts_with("int") => 1.into(),
                    _ if kind.starts_with("list(") => {
                        kind[5..].split(['|', ')']).next().unwrap().into()
                    }
                    _ if kind.contains(r"\d{4}") => "2025-01-01".into(),
                    _ if kind.contains(r"\d{2}:") => "20:00".into(),
                    _ => panic!("unknown schema type {kind}"),
                }
            }
            other => panic!("unexpected schema value {other}"),
        }
    }

    /// The app's `config.yaml`: its default options and a sample of every
    /// option in its schema must parse, with nothing ignored. (An option the
    /// image doesn't know would otherwise be silently dropped.)
    #[test]
    fn the_app_options_match_the_config() {
        use serde_json::Value;
        let app: Value =
            serde_norway::from_str(include_str!("../../../dess_oxide/config.yaml")).unwrap();

        let mut options = app["options"].clone();
        options["victron"]["host"] = "192.168.1.20".into();
        let (config, ignored) = Config::parse(&options.to_string(), true).unwrap();
        assert!(
            ignored.is_empty(),
            "defaults not in the config: {ignored:?}"
        );
        config.validate().unwrap();

        let (config, ignored) = Config::parse(&sample(&app["schema"]).to_string(), true).unwrap();
        assert!(
            ignored.is_empty(),
            "schema options not in the config: {ignored:?}"
        );
        config.validate().unwrap();
    }

    #[test]
    fn parses_the_cheapest_start_window() {
        let config: Config = serde_json::from_str(
            r#"{"victron": {"host": "x"}, "cheapest_start": {"hours": 2.5, "kwh": 0.9, "earliest": "21:30", "finish_by": "07:00"}}"#,
        )
        .unwrap();
        let run = &config.cheapest_start;
        assert_eq!(run.earliest, Time::constant(21, 30, 0, 0));
        assert_eq!(run.finish_by, Time::constant(7, 0, 0, 0));
        assert!(!config.ha_entities);
        assert!(config.dryrun, "a dry run unless set otherwise");
    }

    #[test]
    fn parses_app_options_json() {
        let config: Config = serde_json::from_str(
            r#"{"victron": {"host": "192.168.1.21", "portal_id": "0123456789ab"}}"#,
        )
        .unwrap();
        assert_eq!(config.victron.portal_id.as_deref(), Some("0123456789ab"));
    }

    #[test]
    fn parses_the_example_config() {
        let config: Config = toml::from_str(include_str!("../../../dess.example.toml")).unwrap();
        config.validate().unwrap();
        let tariff = config.tariff.unwrap().to_tariff().unwrap();
        assert_eq!(
            tariff.net_metering_until,
            Some(jiff::civil::date(2026, 12, 31))
        );
        assert_eq!(config.pv.len(), 2);
    }

    #[test]
    fn unknown_options_are_reported_not_fatal() {
        let (config, ignored) = Config::parse(
            r#"{"victron": {"host": "x", "prot": 1}, "future_section": {}}"#,
            true,
        )
        .unwrap();
        assert_eq!(config.victron.host, "x");
        let mut ignored = ignored;
        ignored.sort();
        assert_eq!(ignored, vec!["future_section", "victron.prot"]);
    }

    #[test]
    fn rejects_empty_host() {
        let config: Config = toml::from_str("[victron]\nhost = \" \"\n").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_a_tariff_without_values() {
        let config: Config = toml::from_str(
            "[victron]\nhost = \"x\"\n[tariff]\nvat = []\nenergy_tax = []\nmarkup_buy = []\nmarkup_sell = []\n",
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
