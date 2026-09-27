//! The service: concurrent tasks around one shared state.
//!
//! - **recorder:** samples the system every second and stores 15-minute
//!   energy totals and steady-state efficiency samples;
//! - **prices:** keeps day-ahead prices up to date;
//! - **PV forecast:** refreshes the baseline PV forecast hourly;
//! - **planner:** replans at every slot boundary and whenever prices or the PV
//!   forecast change, and stores each plan (shadow mode);
//! - **history import:** copies hourly energy statistics from Home Assistant;
//! - **web:** serves the dess-oxide page.
//!
//! Nothing here writes to the GX device.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use dess_core::efficiency::EfficiencySampler;
use dess_core::record::{Recorder, SlotRecord};
use dess_core::tariff::Tariff;
use dess_core::{Slot, Watts};
use dess_victron::probe::{ProbeReport, Severity};
use dess_victron::{Snapshot, Venus, VenusOptions, reading};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::nordpool::NordPool;
use crate::planning::{self, PlanView, lock};
use crate::store::Store;
use crate::web;

/// Samples further apart than this are not integrated.
const MAX_SAMPLE_GAP: SignedDuration = SignedDuration::from_secs(10);
/// Without a heartbeat for this long, the snapshot is considered stale.
const MAX_HEARTBEAT_AGE: SignedDuration = SignedDuration::from_secs(15);
/// Full plans are kept this long; after that only each plan's first slot.
const PLAN_RETENTION: SignedDuration = SignedDuration::from_hours(24 * 14);

/// State shared by the tasks and the web page.
pub struct Shared {
    pub config: Config,
    pub tariff: Option<Tariff>,
    pub tz: TimeZone,
    pub store: Mutex<Store>,
    pub plan: watch::Sender<Option<Arc<PlanView>>>,
    pub pv: watch::Sender<Arc<BTreeMap<Slot, Watts>>>,
    pub status: Mutex<Status>,
}

/// What the page shows about the service itself.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// The most pressing current problem, if any.
    pub problem: Option<String>,
    pub findings: Vec<(Severity, String)>,
    pub prices_updated: Option<Timestamp>,
    pub pv_updated: Option<Timestamp>,
}

impl Shared {
    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().expect("status lock poisoned"));
    }
}

pub async fn run(config: Config, data_dir: &Path, listen: SocketAddr) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let store = Store::open(&data_dir.join("dess.db"))?;
    let tariff = config
        .tariff
        .as_ref()
        .map(crate::config::TariffConfig::to_tariff)
        .transpose()?;
    let tz = match &tariff {
        Some(tariff) => tariff.time_zone.clone(),
        None => TimeZone::get("Europe/Amsterdam")?,
    };
    let shared = Arc::new(Shared {
        config,
        tariff,
        tz,
        store: Mutex::new(store),
        plan: watch::Sender::new(None),
        pv: watch::Sender::new(Arc::new(BTreeMap::new())),
        status: Mutex::new(Status::default()),
    });
    let (stop, stopped) = watch::channel(false);
    let web = tokio::spawn(web::serve(listen, Arc::clone(&shared), stopped.clone()));

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let options = VenusOptions {
        host: shared.config.victron.host.clone(),
        port: shared.config.victron.port,
        portal_id: shared.config.victron.portal_id.clone(),
    };
    shared.update_status(|s| s.problem = Some("connecting to the GX device".into()));
    let venus = tokio::select! {
        venus = connect_with_retry(options) => Arc::new(venus),
        () = &mut shutdown => {
            let _ = stop.send(true);
            let _ = web.await;
            return Ok(());
        }
    };
    if let Err(error) = venus.full_publish(Duration::from_secs(30)).await {
        warn!(%error, "continuing without a complete first snapshot");
    }
    log_findings(&venus, &shared);
    wait_for_heartbeat(&venus, Duration::from_secs(10)).await;
    shared.update_status(|s| s.problem = None);

    let mut tasks: Vec<JoinHandle<()>> = vec![
        tokio::spawn(record(
            Arc::clone(&venus),
            Arc::clone(&shared),
            stopped.clone(),
        )),
        tokio::spawn(import_history(Arc::clone(&shared), stopped.clone())),
    ];
    if shared.tariff.is_some() {
        let client = crate::http_client()?;
        let (prices_changed, prices_rx) = watch::channel(0u64);
        tasks.push(tokio::spawn(fetch_prices(
            Arc::clone(&shared),
            client.clone(),
            prices_changed,
            stopped.clone(),
        )));
        tasks.push(tokio::spawn(forecast_pv(
            Arc::clone(&shared),
            client,
            stopped.clone(),
        )));
        tasks.push(tokio::spawn(plan_loop(
            Arc::clone(&venus),
            Arc::clone(&shared),
            prices_rx,
            stopped.clone(),
        )));
    } else {
        info!("no [tariff] configured: recording only, no planning");
    }

    shutdown.await;
    info!("shutting down");
    let _ = stop.send(true);
    for task in tasks {
        let _ = task.await;
    }
    let _ = web.await;
    if let Ok(venus) = Arc::try_unwrap(venus) {
        venus.close().await;
    }
    Ok(())
}

/// Resolves once `stop` becomes true.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopping| *stopping).await;
}

async fn record(venus: Arc<Venus>, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut recorder = Recorder::new(MAX_SAMPLE_GAP);
    let mut sampler = EfficiencySampler::default();
    let mut warnings = RateLimitedWarning::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    info!("recording (read-only)");
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = stopped(&mut stop) => break,
        }
        let now = Timestamp::now();
        let sample = venus.with_snapshot(|snapshot| {
            fresh(&venus, snapshot, now)?;
            reading::sample(snapshot, now).map_err(|e| e.to_string())
        });
        match sample {
            Ok(sample) => {
                if warnings.clear() {
                    shared.update_status(|s| s.problem = None);
                }
                sampler.push(
                    now,
                    sample.inverter_ac,
                    sample.battery - sample.pv_dc,
                    sample.battery_voltage,
                );
                for record in recorder.push(sample) {
                    persist(&shared, &record, &mut sampler, now);
                }
            }
            Err(message) => {
                if warnings.warn(&message) {
                    shared.update_status(|s| s.problem = Some(format!("{message}; not recording")));
                }
            }
        }
    }
    if let Some(record) = recorder.flush() {
        persist(&shared, &record, &mut sampler, Timestamp::now());
    }
}

/// Checks that the snapshot is current.
fn fresh(venus: &Venus, snapshot: &Snapshot, now: Timestamp) -> Result<(), String> {
    let heartbeat_age = snapshot.last_heartbeat().map(|at| now.duration_since(at));
    if !venus.is_connected() || heartbeat_age.is_none_or(|age| age > MAX_HEARTBEAT_AGE) {
        return Err("no recent heartbeat from the GX device".to_owned());
    }
    Ok(())
}

fn persist(shared: &Shared, record: &SlotRecord, sampler: &mut EfficiencySampler, now: Timestamp) {
    let kwh = |wh: dess_core::WattHours| wh.0 / 1000.0;
    info!(
        slot = %record.slot,
        coverage = format!("{:.0}%", record.coverage() * 100.0),
        import_kwh = format!("{:.3}", kwh(record.grid_import)),
        export_kwh = format!("{:.3}", kwh(record.grid_export)),
        pv_kwh = format!("{:.3}", kwh(record.pv_ac + record.pv_dc)),
        load_kwh = format!("{:.3}", kwh(record.load_out + record.load_in)),
        charge_kwh = format!("{:.3}", kwh(record.battery_charge)),
        discharge_kwh = format!("{:.3}", kwh(record.battery_discharge)),
        soc = format!("{:.0}→{:.0}%", record.soc_start, record.soc_end),
        "slot recorded"
    );
    let day = record.slot.start_unix().div_euclid(86_400);
    let result = tokio::task::block_in_place(|| {
        let mut store = lock(&shared.store);
        store.save_slot(record, now.as_second())?;
        store.save_efficiency_bins(day, &sampler.take_bins())
    });
    if let Err(error) = result {
        error!(%error, slot = %record.slot, "failed to store slot");
    }
}

async fn fetch_prices(
    shared: Arc<Shared>,
    client: reqwest::Client,
    changed: watch::Sender<u64>,
    mut stop: watch::Receiver<bool>,
) {
    let nordpool = NordPool::new(client, &shared.config.prices.area);
    loop {
        let now = Timestamp::now();
        let wait = match planning::update_prices(&shared.store, &nordpool, now, &shared.tz).await {
            Ok(update) => {
                shared.update_status(|s| s.prices_updated = Some(now));
                if update.changed {
                    changed.send_modify(|version| *version += 1);
                }
                next_price_check(now, &shared.tz, update.tomorrow_complete)
            }
            Err(error) => {
                warn!("updating prices: {error:#}");
                Duration::from_secs(300)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stopped(&mut stop) => return,
        }
    }
}

/// Every 5 minutes from 12:55 until tomorrow's prices are in, hourly otherwise.
fn next_price_check(now: Timestamp, tz: &TimeZone, tomorrow_complete: bool) -> Duration {
    let publication = now
        .to_zoned(tz.clone())
        .date()
        .at(12, 55, 0, 0)
        .to_zoned(tz.clone())
        .map_or(now, |z| z.timestamp());
    let wait = if !tomorrow_complete && now >= publication {
        SignedDuration::from_mins(5)
    } else if !tomorrow_complete && publication.duration_since(now) < SignedDuration::from_hours(1)
    {
        publication.duration_since(now)
    } else {
        SignedDuration::from_hours(1)
    };
    wait.unsigned_abs().max(Duration::from_secs(1))
}

async fn forecast_pv(
    shared: Arc<Shared>,
    client: reqwest::Client,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let pv = planning::pv_forecast(&client, &shared.config).await;
        if !pv.is_empty() {
            shared.update_status(|s| s.pv_updated = Some(Timestamp::now()));
            shared.pv.send_replace(Arc::new(pv));
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
            () = stopped(&mut stop) => return,
        }
    }
}

async fn plan_loop(
    venus: Arc<Venus>,
    shared: Arc<Shared>,
    mut prices: watch::Receiver<u64>,
    mut stop: watch::Receiver<bool>,
) {
    let mut pv = shared.pv.subscribe();
    loop {
        replan(&venus, &shared);
        let now = Timestamp::now();
        let next = Slot::containing(now).end() + SignedDuration::from_secs(5);
        tokio::select! {
            () = tokio::time::sleep(next.duration_since(now).unsigned_abs()) => {}
            _ = prices.changed() => {}
            _ = pv.changed() => {}
            () = stopped(&mut stop) => return,
        }
    }
}

fn replan(venus: &Venus, shared: &Shared) {
    let Some(tariff) = &shared.tariff else { return };
    let now = Timestamp::now();
    let snapshot = venus.snapshot();
    let pv = shared.pv.borrow().clone();
    let result = fresh(venus, &snapshot, now)
        .map_err(anyhow::Error::msg)
        .and_then(|()| {
            tokio::task::block_in_place(|| {
                let mut store = lock(&shared.store);
                let view =
                    planning::make_plan(now, &snapshot, &store, &shared.config, tariff, &pv)?;
                store.save_plan(now.as_second(), &view.plan.slots, &view.forecasts)?;
                store.prune_plans((now - PLAN_RETENTION).as_second())?;
                anyhow::Ok(view)
            })
        });
    match result {
        Ok(view) => {
            if let Some(first) = view.plan.slots.first() {
                info!(
                    soc = format!("{:.0}%", view.soc),
                    battery_kw = format!("{:+.2}", first.battery_ac.0 / 1000.0),
                    grid_kw = format!("{:+.2}", first.grid.0 / 1000.0),
                    pv_on = first.pv_on,
                    horizon_slots = view.plan.slots.len(),
                    expected_eur = format!("{:.2}", view.plan.expected_cost),
                    "planned (shadow)"
                );
            }
            shared.plan.send_replace(Some(Arc::new(view)));
            shared.update_status(|s| {
                if s.problem
                    .as_deref()
                    .is_some_and(|p| p.starts_with("planning"))
                {
                    s.problem = None;
                }
            });
        }
        Err(error) => {
            warn!("planning: {error:#}");
            shared.update_status(|s| s.problem = Some(format!("planning: {error:#}")));
        }
    }
}

/// Copies hourly energy statistics from Home Assistant: a backfill on the
/// first run, then every six hours whatever is new.
async fn import_history(shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let entities: Vec<String> = shared
        .config
        .history
        .entities()
        .into_iter()
        .map(|(_, e)| e.to_owned())
        .collect();
    if entities.is_empty() {
        return;
    }
    let Some(endpoint) = crate::homeassistant::Endpoint::resolve(&shared.config) else {
        info!("history sensors configured but no Home Assistant connection; not importing");
        return;
    };
    loop {
        match import_statistics(&shared, &endpoint, &entities).await {
            Ok(0) => {}
            Ok(rows) => info!(rows, "imported Home Assistant statistics"),
            Err(error) => warn!("importing Home Assistant statistics: {error:#}"),
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(6 * 3600)) => {}
            () = stopped(&mut stop) => return,
        }
    }
}

async fn import_statistics(
    shared: &Shared,
    endpoint: &crate::homeassistant::Endpoint,
    entities: &[String],
) -> anyhow::Result<usize> {
    const BACKFILL: SignedDuration = SignedDuration::from_hours(24 * 365 * 3);
    const CHUNK: SignedDuration = SignedDuration::from_hours(24 * 30);
    let mut connection = crate::homeassistant::Connection::connect(endpoint).await?;
    let now = Timestamp::now();
    // Only complete hours.
    let end = Timestamp::from_second(now.as_second().div_euclid(3600) * 3600)?;
    let mut total = 0;
    for entity in entities {
        let last = lock(&shared.store).last_ha_hour(entity)?;
        let mut start = last.map_or(now - BACKFILL, |hour| hour + SignedDuration::from_hours(1));
        while start < end {
            let chunk_end = (start + CHUNK).min(end);
            let rows = crate::homeassistant::hourly_energy(
                &mut connection,
                std::slice::from_ref(entity),
                start,
                chunk_end,
            )
            .await?;
            total += rows.len();
            tokio::task::block_in_place(|| lock(&shared.store).save_ha_hourly(&rows))?;
            start = chunk_end;
        }
    }
    Ok(total)
}

/// Keeps trying: after a power cut the GX device may boot slower than Home Assistant.
async fn connect_with_retry(options: VenusOptions) -> Venus {
    let mut backoff = Duration::from_secs(5);
    loop {
        match Venus::connect(options.clone(), Duration::from_secs(30)).await {
            Ok(venus) => return venus,
            Err(error) => {
                warn!(%error, retry_in_s = backoff.as_secs(), "can't reach the GX device");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(300));
            }
        }
    }
}

/// The heartbeat arrives every few seconds; give the first one a moment so
/// the loop doesn't start with a spurious warning.
async fn wait_for_heartbeat(venus: &Venus, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while venus.with_snapshot(|s| s.last_heartbeat().is_none())
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn log_findings(venus: &Venus, shared: &Shared) {
    let report = venus.with_snapshot(|snapshot| {
        ProbeReport::from_snapshot(venus.portal_id(), snapshot, 0.0, Timestamp::now())
    });
    for finding in &report.findings {
        match finding.severity {
            Severity::Info => info!("{}", finding.message),
            Severity::Warning | Severity::Blocker => warn!("{}", finding.message),
        }
    }
    shared.update_status(|s| {
        s.findings = report
            .findings
            .iter()
            .map(|f| (f.severity, f.message.clone()))
            .collect();
    });
}

/// Logs a warning when it changes, and repeats it at most once a minute.
#[derive(Default)]
struct RateLimitedWarning {
    last: Option<(String, std::time::Instant)>,
}

impl RateLimitedWarning {
    /// Returns whether the warning was logged.
    fn warn(&mut self, message: &str) -> bool {
        let repeat = self
            .last
            .as_ref()
            .is_some_and(|(last, at)| last == message && at.elapsed() < Duration::from_secs(60));
        if !repeat {
            warn!("{message}; not recording");
            self.last = Some((message.to_owned(), std::time::Instant::now()));
        }
        !repeat
    }

    /// Returns whether a warning was active.
    fn clear(&mut self) -> bool {
        let was_active = self.last.take().is_some();
        if was_active {
            info!("recording again");
        }
        was_active
    }
}

/// Resolves on Ctrl-C or SIGTERM (how Home Assistant stops apps).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn price_checks_speed_up_around_publication() {
        let tz = TimeZone::get("Europe/Amsterdam").unwrap();
        // 12:30 CEST, tomorrow missing: wait until 12:55.
        assert_eq!(
            next_price_check(at("2026-09-27T10:30:00Z"), &tz, false),
            Duration::from_secs(25 * 60)
        );
        // 13:10 CEST, still missing: every five minutes.
        assert_eq!(
            next_price_check(at("2026-09-27T11:10:00Z"), &tz, false),
            Duration::from_secs(300)
        );
        // Tomorrow complete: hourly.
        assert_eq!(
            next_price_check(at("2026-09-27T11:10:00Z"), &tz, true),
            Duration::from_secs(3600)
        );
    }
}
