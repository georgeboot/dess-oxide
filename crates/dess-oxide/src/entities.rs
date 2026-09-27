//! Optional Home Assistant entities (`ha_entities: true`, docs/DESIGN.md §12.2): a
//! few states for automations, set through HA's REST API.
//!
//! They stay quiet on purpose: a state is sent when it changes, and repeated
//! every 15 minutes so it comes back after HA restarts. Attributes hold
//! nothing that changes on its own, so HA's history gets a row only when the
//! state itself changes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dess_victron::Venus;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::homeassistant::{self, Endpoint};
use crate::run::Shared;

/// How often an unchanged state is sent again.
const REPEAT: Duration = Duration::from_secs(15 * 60);
/// The GX device's active input when the grid is gone.
const SOURCE_DISCONNECTED: f64 = 240.0;

#[derive(Debug, Clone, PartialEq)]
struct Entity {
    id: &'static str,
    state: String,
    attributes: Value,
}

pub async fn publish(
    venus: Arc<Venus>,
    shared: Arc<Shared>,
    client: reqwest::Client,
    mut stop: watch::Receiver<bool>,
) {
    if !shared.config.ha_entities {
        return;
    }
    let Some(endpoint) = Endpoint::resolve(&shared.config) else {
        warn!("ha_entities is on, but there's no Home Assistant to publish to");
        return;
    };
    info!("publishing Home Assistant entities");
    let mut sent: HashMap<&'static str, (Entity, Instant)> = HashMap::new();
    let mut failing = false;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.wait_for(|stopping| *stopping) => return,
        }
        for entity in entities(&venus, &shared) {
            let due = sent
                .get(entity.id)
                .is_none_or(|(last, at)| *last != entity || at.elapsed() >= REPEAT);
            if !due {
                continue;
            }
            let result = homeassistant::set_state(
                &client,
                &endpoint,
                entity.id,
                &entity.state,
                &entity.attributes,
            )
            .await;
            match result {
                Ok(()) => {
                    if failing {
                        info!("publishing Home Assistant entities again");
                        failing = false;
                    }
                    sent.insert(entity.id, (entity, Instant::now()));
                }
                Err(error) => {
                    if !failing {
                        warn!("publishing Home Assistant entities: {error:#}");
                        failing = true;
                    }
                    break;
                }
            }
        }
    }
}

fn entities(venus: &Venus, shared: &Shared) -> Vec<Entity> {
    let start = *shared.cheapest_start.borrow();
    let local = |at: jiff::Timestamp| {
        at.to_zoned(shared.tz.clone())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    };
    let mut entities = vec![Entity {
        id: "sensor.dess_oxide_cheapest_start",
        state: start.map_or_else(|| "unknown".to_owned(), |s| local(s.start)),
        attributes: json!({
            "friendly_name": "dess-oxide cheapest start",
            "device_class": "timestamp",
            "icon": "mdi:dishwasher",
            "end": start.map(|s| local(s.end)),
            "estimated_price": start.map(|s| s.estimated_price),
        }),
    }];
    if let Some(source) = venus.with_snapshot(|s| s.number("system/0/Ac/ActiveIn/Source")) {
        entities.push(Entity {
            id: "binary_sensor.dess_oxide_grid",
            state: if source == SOURCE_DISCONNECTED {
                "off"
            } else {
                "on"
            }
            .to_owned(),
            attributes: json!({
                "friendly_name": "dess-oxide grid",
                "device_class": "power",
            }),
        });
    }
    entities
}
