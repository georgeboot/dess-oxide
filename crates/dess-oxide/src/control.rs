//! Executing the plan (PLAN.md §15): a one-second loop that moves the ESS
//! grid setpoint and, per slot, the PV relay.
//!
//! It only acts when everything below holds, and otherwise releases control
//! back to plain ESS:
//! - both locks are on: `dryrun: false` in the options (the only way to get
//!   write access at all) and the switch on the dess-oxide page;
//! - Victron's Dynamic ESS is off and ESS regulates the total of all phases;
//! - nothing else wrote the ESS setpoint in the last five minutes (such as
//!   DAO's automations: two controllers would fight);
//! - the data is fresh, the grid is up, and a recent plan covers now.

use std::sync::Arc;
use std::time::Duration;

use dess_core::Watts;
use dess_core::control::{self, Decision, Measured};
use dess_victron::writer::{MINIMUM_SOC, SETPOINT_OVERRIDE, SETPOINT_SETTING};
use dess_victron::{Snapshot, Venus, WriteAccess, Writer, reading};
use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::config::RelayAction;
use crate::planning::lock;
use crate::run::Shared;

/// The page switch, stored in the settings table.
pub const SWITCH: &str = "control_enabled";
/// Write the setpoint when it moves this much, or at least this often.
const DEADBAND_W: f64 = 50.0;
const REFRESH: Duration = Duration::from_secs(10);
/// Before an expected outage, ESS's own minimum SoC is raised this long in
/// advance to the planned reserve.
const BACKSTOP_LEAD: SignedDuration = SignedDuration::from_hours(3);
/// The setting that remembers ESS's minimum SoC while the backstop holds it.
const MIN_SOC_BEFORE_OUTAGE: &str = "min_soc_before_outage";

/// A plan older than this isn't trusted.
const MAX_PLAN_AGE: SignedDuration = SignedDuration::from_mins(20);
/// After another writer touches the setpoint, stay away this long.
const FOREIGN_QUIET: SignedDuration = SignedDuration::from_mins(5);

/// A manual override from the page. It lasts until midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Override {
    /// Leave the battery alone; the grid covers everything.
    Hold,
    /// Charge at full power.
    Charge,
    /// Discharge at full power, exporting what the house doesn't use.
    Discharge,
    /// Cover the house from the battery and store surplus PV, like plain ESS.
    SelfConsumption,
}

pub const OVERRIDE_MODE: &str = "override_mode";
pub const OVERRIDE_UNTIL: &str = "override_until";

impl Override {
    pub const ALL: [Self; 4] = [
        Self::Hold,
        Self::Charge,
        Self::Discharge,
        Self::SelfConsumption,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Charge => "charge",
            Self::Discharge => "discharge",
            Self::SelfConsumption => "self_consumption",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hold => "hold the battery",
            Self::Charge => "charge",
            Self::Discharge => "discharge",
            Self::SelfConsumption => "self-consumption",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.key() == key)
    }

    /// The battery AC power it asks for (positive = charging), within the
    /// battery's range and SoC limits.
    pub fn battery_power(
        self,
        measured: Measured,
        battery: &dess_core::battery::BatteryModel,
        min_soc_pct: f64,
    ) -> f64 {
        let can_charge = if measured.soc_pct < 100.0 {
            battery.max_charge_ac.0
        } else {
            0.0
        };
        let can_discharge = if measured.soc_pct > min_soc_pct {
            battery.max_discharge_ac.0
        } else {
            0.0
        };
        match self {
            Self::Hold => 0.0,
            Self::Charge => can_charge,
            Self::Discharge => -can_discharge,
            Self::SelfConsumption => {
                (measured.pv.0 - measured.load.0).clamp(-can_discharge, can_charge)
            }
        }
    }
}

/// The manual override in effect, if any.
pub fn active_override(store: &crate::store::Store, now: Timestamp) -> Option<Override> {
    let until: Timestamp = store.setting(OVERRIDE_UNTIL).ok()??.parse().ok()?;
    if now >= until {
        return None;
    }
    Override::from_key(&store.setting(OVERRIDE_MODE).ok()??)
}

/// What the page shows about control.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ControlStatus {
    /// A dry run (the default): dess-oxide can't write at all.
    #[default]
    Shadow,
    /// Allowed, but not acting, and why.
    Idle(String),
    /// Acting.
    Active(Decision),
}

/// The minimum SoC ESS should hold as an outage backstop now, if any: from
/// three hours before the window until its end, the plan's SoC at the start
/// of the window (a few points less, never above the current SoC, never below
/// ESS's own minimum).
pub fn backstop_target(
    window: Option<(Timestamp, Timestamp)>,
    planned_start_soc: Option<f64>,
    now: Timestamp,
    current_soc: f64,
    original_min: f64,
) -> Option<f64> {
    let (start, end) = window?;
    if now < start - BACKSTOP_LEAD || now >= end {
        return None;
    }
    let target = (planned_start_soc? - 3.0).min(current_soc).floor();
    (target > original_min).then_some(target)
}

/// Whether the page switch is on.
pub fn switched_on(shared: &Shared) -> bool {
    lock(&shared.store)
        .setting(SWITCH)
        .ok()
        .flatten()
        .as_deref()
        == Some("on")
}

pub async fn run(venus: Arc<Venus>, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let Some(access) = WriteAccess::from_config(shared.config.writes_allowed()) else {
        return;
    };
    let writer = venus.writer(access);
    let mut state = State::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!("control is allowed by the options; it acts once the page switch is on");
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.wait_for(|stopping| *stopping) => break,
        }
        let now = Timestamp::now();
        let snapshot = venus.snapshot();
        state.watch_for_foreign_writes(&snapshot, now);
        backstop(&writer, &shared, &snapshot, now).await;
        match evaluate(&venus, &shared, &snapshot, &state, now) {
            Ok(decision) => {
                if !state.active {
                    info!("taking control");
                    state.active = true;
                }
                state.apply(&writer, &shared, decision, now).await;
                shared.set_control(ControlStatus::Active(decision));
            }
            Err(reason) => {
                if state.active {
                    info!(%reason, "releasing control to plain ESS");
                    state.release(&writer, &shared).await;
                }
                shared.set_control(ControlStatus::Idle(reason));
            }
        }
    }
    if state.active {
        info!("shutting down: releasing control to plain ESS");
        state.release(&writer, &shared).await;
    }
}

/// Holds or releases the outage backstop on ESS's minimum SoC. Like the
/// setpoint, it needs both locks (the option and the page switch), but none of
/// the other interlocks: it must hold while the grid is down.
async fn backstop(writer: &Writer<'_>, shared: &Shared, snapshot: &Snapshot, now: Timestamp) {
    let Some(current_min) = snapshot.number(MINIMUM_SOC) else {
        return;
    };
    let switched_on = switched_on(shared);
    let (window, remembered) = {
        let store = lock(&shared.store);
        (
            crate::planning::outage_window(&store, now).filter(|_| switched_on),
            store
                .setting(MIN_SOC_BEFORE_OUTAGE)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok()),
        )
    };
    let original = remembered.unwrap_or(current_min);
    let planned_start_soc = window.and_then(|(start, _)| {
        let view = shared.plan.borrow().clone()?;
        view.plan
            .slots
            .iter()
            .find(|s| s.slot.end() > start)
            .map(|s| s.soc_start)
    });
    let soc = snapshot.number("system/0/Dc/Battery/Soc").unwrap_or(0.0);
    match backstop_target(window, planned_start_soc, now, soc, original) {
        Some(target) if (target - current_min).abs() >= 2.0 => {
            if remembered.is_none() {
                let _ = lock(&shared.store)
                    .set_setting(MIN_SOC_BEFORE_OUTAGE, &current_min.to_string());
            }
            match writer.set_minimum_soc(target).await {
                Ok(()) => info!(target, "raised ESS's minimum SoC as an outage backstop"),
                Err(error) => warn!(%error, "raising the minimum SoC"),
            }
        }
        Some(_) => {}
        None => {
            if let Some(original) = remembered {
                match writer.set_minimum_soc(original).await {
                    Ok(()) => {
                        info!(
                            original,
                            "restored ESS's minimum SoC after the outage window"
                        );
                        let _ = lock(&shared.store).set_setting(MIN_SOC_BEFORE_OUTAGE, "");
                    }
                    Err(error) => warn!(%error, "restoring the minimum SoC"),
                }
            }
        }
    }
}

/// The decision for now, or why there shouldn't be one.
fn evaluate(
    venus: &Venus,
    shared: &Shared,
    snapshot: &Snapshot,
    state: &State,
    now: Timestamp,
) -> Result<Decision, String> {
    if !switched_on(shared) {
        return Err("switched off on this page".into());
    }
    if snapshot
        .number("settings/0/Settings/DynamicEss/Mode")
        .unwrap_or(0.0)
        != 0.0
    {
        return Err("Victron Dynamic ESS is enabled; switch it off first".into());
    }
    if snapshot.number("settings/0/Settings/CGwacs/Hub4Mode") != Some(1.0) {
        return Err("ESS isn't set to regulate the total of all phases (Hub4Mode 1)".into());
    }
    if state
        .foreign_write
        .is_some_and(|at| now.duration_since(at) < FOREIGN_QUIET)
    {
        return Err("something else is writing the ESS setpoint (DAO's automations?); waiting for it to stop".into());
    }
    let heartbeat_age = snapshot.last_heartbeat().map(|at| now.duration_since(at));
    if !venus.is_connected() || heartbeat_age.is_none_or(|age| age > SignedDuration::from_secs(15))
    {
        return Err("no recent data from the GX device".into());
    }
    let sample = reading::sample(snapshot, now).map_err(|e| e.to_string())?;
    if !sample.grid_connected {
        return Err("the grid is down: plain ESS runs the island".into());
    }
    let view = shared.plan.borrow().clone().ok_or("no plan yet")?;
    if now.duration_since(view.planned_at) > MAX_PLAN_AGE {
        return Err("the latest plan is too old".into());
    }
    let measured = Measured {
        load: sample.load_out + sample.load_in,
        pv: sample.pv_ac + sample.pv_dc,
        soc_pct: shared.soc(now).unwrap_or(sample.soc_pct),
    };
    let mut decision = control::decide(
        &view.plan,
        &view.battery,
        &view.settings,
        view.min_soc,
        now,
        measured,
    )
    .ok_or("the plan doesn't cover now")?;
    if let Some(mode) = active_override(&lock(&shared.store), now) {
        let battery_ac = mode.battery_power(measured, &view.battery, view.min_soc);
        decision.battery_ac = Watts(battery_ac);
        decision.setpoint = Watts(measured.load.0 - measured.pv.0 + battery_ac);
    }
    let limit = |w: f64| w.clamp(-view.settings.max_export.0, view.settings.max_import.0);
    decision.setpoint = Watts(limit(decision.setpoint.0));
    Ok(decision)
}

#[derive(Debug, Default)]
struct State {
    active: bool,
    /// What we last wrote, and when.
    written: Option<(f64, std::time::Instant)>,
    relay_closed: Option<bool>,
    /// The setting's change counter when last seen, to notice other writers.
    setting_changes: Option<u32>,
    foreign_write: Option<Timestamp>,
}

impl State {
    /// Notices someone else writing the setpoint: the persisted setting
    /// changing, or the override holding a value we didn't write.
    fn watch_for_foreign_writes(&mut self, snapshot: &Snapshot, now: Timestamp) {
        let changes = snapshot.changes(SETPOINT_SETTING);
        if self.setting_changes.is_some_and(|seen| changes > seen) {
            self.foreign_write = Some(now);
        }
        self.setting_changes = Some(changes);
        let override_value = snapshot.number(SETPOINT_OVERRIDE);
        let ours = self.written.map(|(w, _)| w);
        // Releasing may leave the override at ESS's own setting: that's plain ESS.
        let setting = snapshot.number(SETPOINT_SETTING);
        if let Some(value) = override_value.filter(|v| setting.is_none_or(|s| (s - v).abs() > 1.0))
        {
            let recently_written = self
                .written
                .is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(5));
            if !recently_written && ours.is_none_or(|w| (w - value).abs() > 1.0) {
                self.foreign_write = Some(now);
            }
        }
    }

    async fn apply(
        &mut self,
        writer: &Writer<'_>,
        shared: &Shared,
        decision: Decision,
        now: Timestamp,
    ) {
        let setpoint = decision.setpoint.0.round();
        let due = self
            .written
            .is_none_or(|(w, at)| (w - setpoint).abs() > DEADBAND_W || at.elapsed() > REFRESH);
        if due {
            match writer.set_setpoint(Some(setpoint)).await {
                Ok(()) => self.written = Some((setpoint, std::time::Instant::now())),
                Err(error) => warn!(%error, "writing the setpoint"),
            }
        }
        if let Some(relay) = shared.config.victron.pv_relay {
            let closed = relay_closed_for(decision.pv_on, shared.config.victron.pv_relay_energized);
            if self.relay_closed != Some(closed) {
                match writer.set_relay(relay, closed).await {
                    Ok(()) => {
                        info!(pv_on = decision.pv_on, %now, "switched the PV relay");
                        self.relay_closed = Some(closed);
                    }
                    Err(error) => warn!(%error, "switching the PV relay"),
                }
            }
        }
    }

    /// Plain ESS again: the override released, PV on.
    async fn release(&mut self, writer: &Writer<'_>, shared: &Shared) {
        if let Err(error) = writer.set_setpoint(None).await {
            error!(%error, "releasing the setpoint override");
        }
        if let Some(relay) = shared.config.victron.pv_relay {
            let closed = relay_closed_for(true, shared.config.victron.pv_relay_energized);
            if let Err(error) = writer.set_relay(relay, closed).await {
                error!(%error, "switching the PV back on");
            }
        }
        self.active = false;
        self.written = None;
        self.relay_closed = None;
    }
}

fn relay_closed_for(pv_on: bool, energized: RelayAction) -> bool {
    match energized {
        RelayAction::PvOff => !pv_on,
        RelayAction::PvOn => pv_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dess_victron::Value;

    #[test]
    fn the_backstop_holds_the_reserve_around_the_window() {
        let start: Timestamp = "2026-09-28T06:00:00Z".parse().unwrap();
        let end = start + SignedDuration::from_hours(4);
        let window = Some((start, end));
        let at = |h: i64| start + SignedDuration::from_hours(h);
        assert_eq!(
            backstop_target(window, Some(60.0), at(-4), 70.0, 5.0),
            None,
            "too early"
        );
        assert_eq!(
            backstop_target(window, Some(60.0), at(-2), 70.0, 5.0),
            Some(57.0)
        );
        assert_eq!(
            backstop_target(window, Some(60.0), at(-2), 40.0, 5.0),
            Some(40.0),
            "never above the current SoC"
        );
        assert_eq!(
            backstop_target(window, Some(60.0), at(2), 50.0, 5.0),
            Some(50.0),
            "holds during the outage"
        );
        assert_eq!(
            backstop_target(window, Some(60.0), at(4), 50.0, 5.0),
            None,
            "over"
        );
        assert_eq!(
            backstop_target(window, Some(6.0), at(-1), 50.0, 5.0),
            None,
            "not above ESS's own minimum"
        );
        assert_eq!(backstop_target(None, Some(60.0), at(-1), 50.0, 5.0), None);
    }

    #[test]
    fn overrides_stay_within_the_battery() {
        let battery = dess_core::battery::BatteryModel::multiplus_ii_prior(
            dess_core::WattHours(10_000.0),
            3,
            Watts(9000.0),
            Watts(12_000.0),
        );
        let measured = |soc_pct| Measured {
            load: Watts(1500.0),
            pv: Watts(500.0),
            soc_pct,
        };
        let power = |mode: Override, soc| mode.battery_power(measured(soc), &battery, 20.0);
        assert_eq!(power(Override::Hold, 50.0), 0.0);
        assert!(power(Override::Charge, 50.0) > 0.0);
        assert_eq!(power(Override::Charge, 100.0), 0.0);
        assert!(power(Override::Discharge, 50.0) < 0.0);
        assert_eq!(power(Override::Discharge, 20.0), 0.0);
        assert_eq!(power(Override::SelfConsumption, 50.0), -1000.0);
        assert_eq!(power(Override::SelfConsumption, 15.0), 0.0);
        for mode in Override::ALL {
            assert_eq!(Override::from_key(mode.key()), Some(mode));
        }
    }

    #[test]
    fn relay_follows_the_wiring() {
        assert!(!relay_closed_for(true, RelayAction::PvOff));
        assert!(relay_closed_for(false, RelayAction::PvOff));
        assert!(relay_closed_for(true, RelayAction::PvOn));
    }

    #[test]
    fn notices_dao_writing_the_setting() {
        let mut state = State::default();
        let mut snapshot = Snapshot::default();
        let now = Timestamp::UNIX_EPOCH;
        snapshot.update(SETPOINT_SETTING, Value::Number(100.0), now);
        state.watch_for_foreign_writes(&snapshot, now);
        assert!(
            state.foreign_write.is_none(),
            "the first sighting is a baseline"
        );
        snapshot.update(SETPOINT_SETTING, Value::Number(2500.0), now);
        state.watch_for_foreign_writes(&snapshot, now);
        assert_eq!(state.foreign_write, Some(now));
    }

    #[test]
    fn an_override_at_the_setting_is_plain_ess() {
        let mut state = State::default();
        let mut snapshot = Snapshot::default();
        snapshot.update(SETPOINT_SETTING, Value::Number(50.0), Timestamp::UNIX_EPOCH);
        snapshot.update(
            SETPOINT_OVERRIDE,
            Value::Number(50.0),
            Timestamp::UNIX_EPOCH,
        );
        state.watch_for_foreign_writes(&snapshot, Timestamp::UNIX_EPOCH);
        assert!(state.foreign_write.is_none());
    }

    #[test]
    fn notices_a_foreign_override() {
        let mut state = State::default();
        let mut snapshot = Snapshot::default();
        snapshot.update(
            SETPOINT_OVERRIDE,
            Value::Number(-3000.0),
            Timestamp::UNIX_EPOCH,
        );
        state.watch_for_foreign_writes(&snapshot, Timestamp::UNIX_EPOCH);
        assert!(state.foreign_write.is_some());
    }
}
