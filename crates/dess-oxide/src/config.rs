//! Configuration: `/data/options.json` in the Home Assistant app, or a TOML
//! file when running standalone. Both use the same schema.

use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub victron: VictronConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VictronConfig {
    /// Host name or IP address of the GX device.
    pub host: String,
    /// Port of the GX device's MQTT broker.
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    /// VRM portal id; discovered when not set.
    #[serde(default)]
    pub portal_id: Option<String>,
}

fn default_mqtt_port() -> u16 {
    1883
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = if path.extension().is_some_and(|ext| ext == "json") {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.victron.host.trim().is_empty() {
            bail!("victron.host is empty; set it to the GX device's address");
        }
        Ok(())
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
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[victron]\nhost = \"x\"\nprot = 1\n").is_err());
    }

    #[test]
    fn rejects_empty_host() {
        let config: Config = toml::from_str("[victron]\nhost = \" \"\n").unwrap();
        assert!(config.validate().is_err());
    }
}
