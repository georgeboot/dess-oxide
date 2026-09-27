use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::snapshot::Snapshot;
use crate::value::Value;

/// Keep-alives expire after 60 s on the GX side; refresh well before that.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Periodically ask for everything again, in case a change notification got lost.
const FULL_PUBLISH_INTERVAL: Duration = Duration::from_secs(600);
const SUPPRESS_REPUBLISH: &str = r#"{"keepalive-options":["suppress-republish"]}"#;

#[derive(Debug, Clone)]
pub struct VenusOptions {
    pub host: String,
    pub port: u16,
    /// The VRM portal id. Discovered from the broker when not set.
    pub portal_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum VenusError {
    #[error(
        "no heartbeat from the GX device at {host}:{port} within {seconds} s. \
         Is \"MQTT on LAN\" enabled (Settings → Integrations → MQTT)? Setting the portal id \
         (Settings → VRM online portal) skips discovery."
    )]
    Discovery {
        host: String,
        port: u16,
        seconds: u64,
    },
    #[error("the GX device did not finish publishing all values within {seconds} s")]
    FullPublishTimeout { seconds: u64 },
    #[error("MQTT client: {0}")]
    Client(#[from] rumqttc::ClientError),
}

/// A connection to a GX device's local MQTT broker.
///
/// A background task keeps the [`Snapshot`] current, reconnects after
/// failures and sends keep-alives. The only messages this client publishes
/// are `R/…` read requests.
pub struct Venus {
    client: AsyncClient,
    shared: Arc<Shared>,
    portal_id: String,
    tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    snapshot: RwLock<Snapshot>,
    portal: watch::Sender<Option<String>>,
    full_publish_echo: watch::Sender<Option<String>>,
    connected: AtomicBool,
    /// Set by [`Venus::close`], so the event loop doesn't report our own disconnect.
    closing: AtomicBool,
}

impl Venus {
    /// Connects, discovers the portal id if needed and requests all values.
    ///
    /// Returns once the portal id is known; values keep arriving in the
    /// background. Use [`Venus::full_publish`] to wait for a complete snapshot.
    pub async fn connect(
        options: VenusOptions,
        discovery_timeout: Duration,
    ) -> Result<Self, VenusError> {
        let client_id = format!(
            "dess-oxide-{}-{}",
            std::process::id(),
            Timestamp::now().as_millisecond()
        );
        let mut mqtt = MqttOptions::new(client_id, options.host.clone(), options.port);
        mqtt.set_keep_alive(Duration::from_secs(30));
        mqtt.set_max_packet_size(1 << 20, 1 << 16);
        let (client, eventloop) = AsyncClient::new(mqtt, 1024);

        let shared = Arc::new(Shared {
            snapshot: RwLock::new(Snapshot::default()),
            portal: watch::Sender::new(options.portal_id.clone()),
            full_publish_echo: watch::Sender::new(None),
            connected: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        });

        let mut portal_rx = shared.portal.subscribe();
        let event_task = tokio::spawn(run_event_loop(
            eventloop,
            client.clone(),
            Arc::clone(&shared),
        ));

        let discovered =
            tokio::time::timeout(discovery_timeout, portal_rx.wait_for(Option::is_some)).await;
        let Ok(Ok(portal)) = discovered else {
            event_task.abort();
            return Err(VenusError::Discovery {
                host: options.host,
                port: options.port,
                seconds: discovery_timeout.as_secs(),
            });
        };
        let portal_id = portal.clone().expect("waited for Some");
        info!(portal_id, host = %options.host, "connected to GX device");

        let keepalive_task = tokio::spawn(run_keepalive(client.clone(), portal_id.clone()));
        Ok(Self {
            client,
            shared,
            portal_id,
            tasks: vec![event_task, keepalive_task],
        })
    }

    pub fn portal_id(&self) -> &str {
        &self.portal_id
    }

    /// Whether the MQTT connection is currently up.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// Runs `f` against the current snapshot.
    pub fn with_snapshot<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        f(&self.shared.snapshot.read().expect("snapshot lock poisoned"))
    }

    /// A copy of the current snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.with_snapshot(Clone::clone)
    }

    /// Asks the GX device to republish every value and waits until it has.
    pub async fn full_publish(&self, timeout: Duration) -> Result<(), VenusError> {
        let token = format!("dess-oxide-{}", Timestamp::now().as_nanosecond());
        let mut echo = self.shared.full_publish_echo.subscribe();
        let payload =
            serde_json::json!({ "keepalive-options": [{ "full-publish-completed-echo": token }] });
        self.client
            .publish(
                format!("R/{}/keepalive", self.portal_id),
                QoS::AtMostOnce,
                false,
                payload.to_string(),
            )
            .await?;
        tokio::time::timeout(
            timeout,
            echo.wait_for(|e| e.as_deref() == Some(token.as_str())),
        )
        .await
        .map(|_| ())
        .map_err(|_| VenusError::FullPublishTimeout {
            seconds: timeout.as_secs(),
        })
    }

    /// Disconnects cleanly.
    pub async fn close(self) {
        self.shared.closing.store(true, Ordering::Relaxed);
        let _ = self.client.disconnect().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

impl Drop for Venus {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn run_event_loop(mut eventloop: EventLoop, client: AsyncClient, shared: Arc<Shared>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                backoff = Duration::from_secs(1);
                shared.connected.store(true, Ordering::Relaxed);
                // Clean sessions: subscriptions have to be renewed on every connect.
                match shared.portal.borrow().clone() {
                    Some(portal) => subscribe_portal(&client, &portal),
                    None => {
                        for topic in ["N/+/heartbeat", "N/+/system/0/Serial"] {
                            try_or_warn(client.try_subscribe(topic, QoS::AtMostOnce));
                        }
                    }
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let Some((portal, key)) = split_topic(&publish.topic) else {
                    continue;
                };
                if shared.portal.borrow().is_none() {
                    info!(portal, "discovered portal id");
                    shared.portal.send_replace(Some(portal.to_owned()));
                    subscribe_portal(&client, portal);
                }
                if shared.portal.borrow().as_deref() != Some(portal) {
                    continue;
                }
                if key == "full_publish_completed" {
                    record_full_publish_echo(&shared, &publish.payload);
                }
                let value = Value::parse(&publish.payload);
                shared
                    .snapshot
                    .write()
                    .expect("snapshot lock poisoned")
                    .update(key, value, Timestamp::now());
            }
            Ok(_) => {}
            Err(_) if shared.closing.load(Ordering::Relaxed) => return,
            Err(error) => {
                if shared.connected.swap(false, Ordering::Relaxed) {
                    warn!(%error, "lost connection to GX device, reconnecting");
                } else {
                    debug!(%error, "reconnect failed");
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Subscribes to everything the portal publishes and asks for a full republish.
fn subscribe_portal(client: &AsyncClient, portal: &str) {
    try_or_warn(client.try_subscribe(format!("N/{portal}/#"), QoS::AtMostOnce));
    try_or_warn(client.try_publish(format!("R/{portal}/keepalive"), QoS::AtMostOnce, false, ""));
}

fn record_full_publish_echo(shared: &Shared, payload: &[u8]) {
    let echo = serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("full-publish-completed-echo")?
                .as_str()
                .map(str::to_owned)
        });
    if echo.is_some() {
        shared.full_publish_echo.send_replace(echo);
    }
}

async fn run_keepalive(client: AsyncClient, portal: String) {
    let topic = format!("R/{portal}/keepalive");
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    let mut full = tokio::time::interval(FULL_PUBLISH_INTERVAL);
    // Both fire immediately; the connect handler already requested a full publish.
    keepalive.tick().await;
    full.tick().await;
    loop {
        let payload = tokio::select! {
            _ = keepalive.tick() => SUPPRESS_REPUBLISH,
            _ = full.tick() => "",
        };
        if let Err(error) = client
            .publish(topic.as_str(), QoS::AtMostOnce, false, payload)
            .await
        {
            warn!(%error, "failed to send keep-alive");
        }
    }
}

fn try_or_warn(result: Result<(), rumqttc::ClientError>) {
    if let Err(error) = result {
        warn!(%error, "MQTT request queue is full");
    }
}

/// Splits `N/<portal>/<rest>` into the portal id and `<rest>`.
fn split_topic(topic: &str) -> Option<(&str, &str)> {
    let rest = topic.strip_prefix("N/")?;
    rest.split_once('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_notification_topics() {
        assert_eq!(
            split_topic("N/0123456789ab/system/0/Dc/Battery/Soc"),
            Some(("0123456789ab", "system/0/Dc/Battery/Soc"))
        );
        assert_eq!(
            split_topic("N/0123456789ab/heartbeat"),
            Some(("0123456789ab", "heartbeat"))
        );
        assert_eq!(split_topic("W/0123456789ab/system/0/Relay/1/State"), None);
    }
}
