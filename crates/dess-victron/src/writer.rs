//! Writing to the GX device: a separate capability from reading.
//!
//! A [`Writer`] needs a [`WriteAccess`], and only the `control: true`
//! configuration option creates one, so code without it can't write at all.
//! Writes are limited to three paths, and each is confirmed by reading the
//! value back from the GX device.

use std::time::Duration;

use rumqttc::QoS;
use serde_json::{Value as Json, json};

use crate::client::{Venus, VenusError};
use crate::value::Value;

/// The volatile ESS grid setpoint override, W (positive = import). `null`
/// releases it, and ESS falls back to its own setpoint setting.
pub const SETPOINT_OVERRIDE: &str = "hub4/0/Overrides/Setpoint";
/// ESS's minimum SoC (%).
pub const MINIMUM_SOC: &str = "settings/0/Settings/CGwacs/BatteryLife/MinimumSocLimit";
/// ESS's persisted setpoint setting; read to fall back to when releasing.
pub const SETPOINT_SETTING: &str = "settings/0/Settings/CGwacs/AcPowerSetPoint";

/// How long the GX device gets to echo a written value.
const READBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Proof that writing was enabled in the configuration.
#[derive(Debug)]
pub struct WriteAccess(());

impl WriteAccess {
    /// Only the `control: true` option grants write access.
    pub fn from_config(control_enabled: bool) -> Option<Self> {
        control_enabled.then_some(Self(()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("the GX device didn't confirm {path} = {expected} within {seconds} s")]
    NotConfirmed {
        path: String,
        expected: String,
        seconds: u64,
    },
    #[error(transparent)]
    Venus(#[from] VenusError),
    #[error("MQTT client: {0}")]
    Client(#[from] rumqttc::ClientError),
}

/// Writes the few values dess-oxide controls.
pub struct Writer<'a> {
    venus: &'a Venus,
}

impl Venus {
    /// The write capability.
    pub fn writer(&self, _access: WriteAccess) -> Writer<'_> {
        Writer { venus: self }
    }
}

impl Writer<'_> {
    /// Sets the grid setpoint override (W, positive = import), or releases it.
    ///
    /// Releasing writes `null`; if the GX device doesn't take that, the
    /// override is set to ESS's own setpoint setting, which behaves the same.
    pub async fn set_setpoint(&self, watts: Option<f64>) -> Result<(), WriteError> {
        match watts {
            Some(w) => self.write(SETPOINT_OVERRIDE, json!(w.round())).await,
            None => match self.write(SETPOINT_OVERRIDE, Json::Null).await {
                Ok(()) => Ok(()),
                Err(WriteError::NotConfirmed { .. }) => {
                    let setting = self
                        .venus
                        .with_snapshot(|s| s.number(SETPOINT_SETTING))
                        .unwrap_or(0.0);
                    self.write(SETPOINT_OVERRIDE, json!(setting.round())).await
                }
                Err(error) => Err(error),
            },
        }
    }

    /// Closes (energises) or opens Cerbo relay `number` (1 or 2).
    pub async fn set_relay(&self, number: u8, closed: bool) -> Result<(), WriteError> {
        let path = format!("system/0/Relay/{}/State", number.clamp(1, 2) - 1);
        self.write(&path, json!(u8::from(closed))).await
    }

    /// Sets ESS's minimum SoC, %.
    pub async fn set_minimum_soc(&self, percent: f64) -> Result<(), WriteError> {
        self.write(MINIMUM_SOC, json!(percent.clamp(0.0, 100.0).round()))
            .await
    }

    async fn write(&self, path: &str, value: Json) -> Result<(), WriteError> {
        let topic = format!("W/{}/{path}", self.venus.portal_id());
        let payload = json!({ "value": value }).to_string();
        self.venus
            .client()
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await?;
        // Ask for the value again, and wait until the device reports it.
        self.venus.request(path).await?;
        let expected = Value::parse(json!({ "value": value }).to_string().as_bytes());
        let deadline = tokio::time::Instant::now() + READBACK_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let current = self
                .venus
                .with_snapshot(|s| s.get(path).map(|e| e.value.clone()));
            if current.as_ref().is_some_and(|v| same(v, &expected)) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(WriteError::NotConfirmed {
            path: path.to_owned(),
            expected: value.to_string(),
            seconds: READBACK_TIMEOUT.as_secs(),
        })
    }
}

/// Numbers compare with a little tolerance (the device may round).
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() < 0.5,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_config_grants_access() {
        assert!(WriteAccess::from_config(false).is_none());
        assert!(WriteAccess::from_config(true).is_some());
    }

    #[test]
    fn readback_tolerates_rounding() {
        assert!(same(&Value::Number(1500.0), &Value::Number(1500.3)));
        assert!(!same(&Value::Number(1500.0), &Value::Number(1501.0)));
        assert!(same(&Value::Null, &Value::Null));
        assert!(!same(&Value::Null, &Value::Number(0.0)));
    }
}
