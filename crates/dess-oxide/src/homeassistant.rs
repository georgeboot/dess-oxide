//! What dess-oxide reads from Home Assistant: its location, and the
//! long-term statistics of energy sensors (to bootstrap history). With
//! `ha_entities: true` it also sets the state of a few entities of its own.
//!
//! Inside HA the app talks to the Supervisor's proxy with `SUPERVISOR_TOKEN`;
//! standalone it needs `[homeassistant] url` and a long-lived `token`. Only
//! plain `http`/`ws` is supported, which is all the Supervisor proxy needs.

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::config::{Config, LocationConfig};

/// Where Home Assistant's APIs are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Base of the REST API, e.g. `http://supervisor/core/api`.
    pub rest: String,
    pub websocket: String,
    pub token: String,
}

impl Endpoint {
    /// The Supervisor's proxy inside HA, else `[homeassistant]` from the config.
    pub fn resolve(config: &Config) -> Option<Self> {
        if let Ok(token) = std::env::var("SUPERVISOR_TOKEN") {
            return Some(Self {
                rest: "http://supervisor/core/api".to_owned(),
                websocket: "ws://supervisor/core/websocket".to_owned(),
                token,
            });
        }
        let ha = config.homeassistant.as_ref()?;
        let base = ha.url.trim_end_matches('/');
        Some(Self {
            rest: format!("{base}/api"),
            websocket: format!("{}/api/websocket", base.replacen("http", "ws", 1)),
            token: ha.token.clone(),
        })
    }
}

/// Home Assistant's configured location.
pub async fn location(
    client: &reqwest::Client,
    endpoint: &Endpoint,
) -> anyhow::Result<LocationConfig> {
    #[derive(Deserialize)]
    struct CoreConfig {
        latitude: f64,
        longitude: f64,
    }
    let config: CoreConfig = client
        .get(format!("{}/config", endpoint.rest))
        .bearer_auth(&endpoint.token)
        .send()
        .await
        .context("asking Home Assistant for its location")?
        .error_for_status()?
        .json()
        .await
        .context("parsing Home Assistant's config")?;
    Ok(LocationConfig {
        latitude: config.latitude,
        longitude: config.longitude,
    })
}

/// Sets an entity's state through the REST API. HA forgets such states when
/// it restarts, so callers repeat them now and then; an unchanged state
/// doesn't add to HA's history.
pub async fn set_state(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    entity_id: &str,
    state: &str,
    attributes: &Value,
) -> anyhow::Result<()> {
    client
        .post(format!("{}/states/{entity_id}", endpoint.rest))
        .bearer_auth(&endpoint.token)
        .json(&json!({ "state": state, "attributes": attributes }))
        .send()
        .await
        .with_context(|| format!("setting {entity_id}"))?
        .error_for_status()?;
    Ok(())
}

/// An authenticated WebSocket connection.
pub struct Connection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Connection {
    pub async fn connect(endpoint: &Endpoint) -> anyhow::Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(endpoint.websocket.as_str())
            .await
            .with_context(|| format!("connecting to {}", endpoint.websocket))?;
        let mut connection = Self { socket, next_id: 1 };
        let greeting = connection.receive().await?;
        if greeting["type"] != "auth_required" {
            bail!("unexpected greeting from Home Assistant: {greeting}");
        }
        connection
            .send(json!({ "type": "auth", "access_token": endpoint.token }))
            .await?;
        let reply = connection.receive().await?;
        if reply["type"] != "auth_ok" {
            bail!("Home Assistant refused the token: {}", reply["message"]);
        }
        Ok(connection)
    }

    /// Sends a command and returns its `result`.
    pub async fn call(&mut self, mut command: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        command["id"] = json!(id);
        self.send(command).await?;
        loop {
            let message = self.receive().await?;
            if message["id"] != json!(id) || message["type"] != "result" {
                continue;
            }
            if message["success"] != json!(true) {
                bail!("Home Assistant: {}", message["error"]);
            }
            return Ok(message["result"].clone());
        }
    }

    async fn send(&mut self, message: Value) -> anyhow::Result<()> {
        self.socket.send(Message::text(message.to_string())).await?;
        Ok(())
    }

    async fn receive(&mut self) -> anyhow::Result<Value> {
        loop {
            match self
                .socket
                .next()
                .await
                .context("Home Assistant closed the connection")??
            {
                Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                Message::Close(_) => bail!("Home Assistant closed the connection"),
                _ => {}
            }
        }
    }
}

/// Hourly change of energy statistics (kWh) in `[start, end)`.
pub async fn hourly_energy(
    connection: &mut Connection,
    statistic_ids: &[String],
    start: Timestamp,
    end: Timestamp,
) -> anyhow::Result<Vec<(String, Timestamp, f64)>> {
    let result = connection
        .call(json!({
            "type": "recorder/statistics_during_period",
            "start_time": start.to_string(),
            "end_time": end.to_string(),
            "statistic_ids": statistic_ids,
            "period": "hour",
            "types": ["change"],
            "units": { "energy": "kWh" },
        }))
        .await?;
    parse_hourly(&result)
}

fn parse_hourly(result: &Value) -> anyhow::Result<Vec<(String, Timestamp, f64)>> {
    let mut rows = Vec::new();
    let Some(by_id) = result.as_object() else {
        bail!("unexpected statistics result: {result}");
    };
    for (id, entries) in by_id {
        for entry in entries.as_array().into_iter().flatten() {
            // `start` is milliseconds since the epoch (a float in recent versions).
            let (Some(start_ms), Some(change)) =
                (entry["start"].as_f64(), entry["change"].as_f64())
            else {
                continue;
            };
            rows.push((
                id.clone(),
                Timestamp::from_millisecond(start_ms as i64)?,
                change,
            ));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statistics() {
        let result = json!({
            "sensor.p1_import": [
                { "start": 1_790_000_000_000.0_f64, "end": 1_790_003_600_000.0_f64, "change": 1.25 },
                { "start": 1_790_003_600_000.0_f64, "end": 1_790_007_200_000.0_f64, "change": null },
            ],
        });
        let rows = parse_hourly(&result).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, 1.25);
        assert_eq!(rows[0].1, Timestamp::from_second(1_790_000_000).unwrap());
    }

    #[tokio::test]
    async fn authenticates_and_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // A tiny fake Home Assistant.
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text(r#"{"type":"auth_required"}"#))
                .await
                .unwrap();
            let auth: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(auth["access_token"], "secret");
            ws.send(Message::text(r#"{"type":"auth_ok"}"#))
                .await
                .unwrap();
            let call: Value =
                serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            ws.send(Message::text(r#"{"type":"event","id":99}"#))
                .await
                .unwrap();
            let reply = json!({ "id": call["id"], "type": "result", "success": true, "result": { "ok": 1 } });
            ws.send(Message::text(reply.to_string())).await.unwrap();
        });
        let endpoint = Endpoint {
            rest: String::new(),
            websocket: format!("ws://{address}"),
            token: "secret".into(),
        };
        let mut connection = Connection::connect(&endpoint).await.unwrap();
        let result = connection.call(json!({ "type": "ping" })).await.unwrap();
        assert_eq!(result["ok"], 1);
    }
}
