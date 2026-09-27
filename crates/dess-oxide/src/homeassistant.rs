//! What dess-oxide reads from Home Assistant through the Supervisor.

use anyhow::Context;
use serde::Deserialize;

use crate::config::LocationConfig;

/// Home Assistant's configured location, when running as an app.
///
/// Returns `None` outside Home Assistant (no `SUPERVISOR_TOKEN`).
pub async fn location(client: &reqwest::Client) -> anyhow::Result<Option<LocationConfig>> {
    #[derive(Deserialize)]
    struct CoreConfig {
        latitude: f64,
        longitude: f64,
    }
    let Ok(token) = std::env::var("SUPERVISOR_TOKEN") else {
        return Ok(None);
    };
    let config: CoreConfig = client
        .get("http://supervisor/core/api/config")
        .bearer_auth(token)
        .send()
        .await
        .context("asking Home Assistant for its location")?
        .error_for_status()?
        .json()
        .await
        .context("parsing Home Assistant's config")?;
    Ok(Some(LocationConfig {
        latitude: config.latitude,
        longitude: config.longitude,
    }))
}
