//! A PV contactor behind Home Assistant switches (a Shelly, say), for sites
//! where the GX device's relay doesn't drive it.
//!
//! Their state is read every few seconds, so the recordings and the plan
//! know whether PV is on. The executor asks for a change through
//! [`request`]; this task makes it happen, and keeps it so while dess-oxide
//! is in control. Like the GX relay, the switches are only operated with
//! both locks on.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::control::ControlStatus;
use crate::homeassistant::{self, Endpoint};
use crate::planning::lock;
use crate::run::Shared;

const POLL: Duration = Duration::from_secs(5);
/// "on" while dess-oxide has the PV off, so it can be turned back on after a
/// restart or at shutdown: unlike the GX relay, a switch doesn't fall back
/// to PV on by itself.
const OFF_MARKER: &str = "pv_off_by_dess";

/// Asks for the PV to be on or off.
pub fn request(shared: &Shared, pv_on: bool) {
    let on = shared.config.pv_switch.on_for(pv_on);
    let changed = shared.pv_switch_wanted.send_if_modified(|wanted| {
        let changed = *wanted != Some(on);
        *wanted = Some(on);
        changed
    });
    if changed {
        let marker = if pv_on { "off" } else { "on" };
        if let Err(error) = lock(&shared.store).set_setting(OFF_MARKER, marker) {
            warn!(%error, "remembering the PV switch's state");
        }
    }
}

fn switched_off_by_us(shared: &Shared) -> bool {
    lock(&shared.store)
        .setting(OFF_MARKER)
        .ok()
        .flatten()
        .as_deref()
        == Some("on")
}

/// Whether the switches are on: `None` when they disagree, or one is
/// unavailable.
async fn read(client: &reqwest::Client, endpoint: &Endpoint, entities: &[String]) -> Option<bool> {
    let mut all = None;
    for entity in entities {
        let on = match homeassistant::state(client, endpoint, entity).await {
            Ok(Some(state)) if state == "on" => true,
            Ok(Some(state)) if state == "off" => false,
            Ok(_) => return None,
            Err(error) => {
                warn!("reading the PV switch: {error:#}");
                return None;
            }
        };
        if all.is_some_and(|other| other != on) {
            return None;
        }
        all = Some(on);
    }
    all
}

async fn turn(client: &reqwest::Client, endpoint: &Endpoint, entities: &[String], on: bool) {
    for entity in entities {
        match homeassistant::turn(client, endpoint, entity, on).await {
            Ok(()) => info!(entity, on, "switched the PV switch"),
            Err(error) => warn!("switching the PV switch: {error:#}"),
        }
    }
}

pub async fn run(shared: Arc<Shared>, client: reqwest::Client, mut stop: watch::Receiver<bool>) {
    let entities: Vec<String> = shared
        .config
        .pv_switch
        .entities()
        .into_iter()
        .map(str::to_owned)
        .collect();
    if entities.is_empty() {
        return;
    }
    let Some(endpoint) = Endpoint::resolve(&shared.config) else {
        warn!("pv_switch is set, but there's no Home Assistant connection to reach it");
        return;
    };
    let may_switch = shared.config.writes_allowed();
    // PV that was off when dess-oxide last stopped: back on, until the
    // executor decides otherwise.
    if may_switch && switched_off_by_us(&shared) {
        request(&shared, true);
    }
    let mut wanted = shared.pv_switch_wanted.subscribe();
    loop {
        let state = read(&client, &endpoint, &entities).await;
        *shared.pv_switch.lock().expect("lock poisoned") = state.map(|on| (Timestamp::now(), on));
        let want = *wanted.borrow_and_update();
        if let Some(on) = want.filter(|_| may_switch) {
            if state != Some(on) {
                turn(&client, &endpoint, &entities, on).await;
            } else if !matches!(shared.control_status(), ControlStatus::Active(_)) {
                // Done, and no longer ours to hold: it may be switched by hand.
                shared.pv_switch_wanted.send_replace(None);
            }
        }
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            _ = wanted.changed() => {}
            _ = stop.wait_for(|stopping| *stopping) => break,
        }
    }
    // Stopping with the PV off by our doing: on again.
    if may_switch && switched_off_by_us(&shared) {
        turn(
            &client,
            &endpoint,
            &entities,
            shared.config.pv_switch.on_for(true),
        )
        .await;
        let _ = lock(&shared.store).set_setting(OFF_MARKER, "off");
    }
}
