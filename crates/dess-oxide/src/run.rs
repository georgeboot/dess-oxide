//! The service: concurrent tasks around one shared state.
//!
//! - **recorder:** samples the system every second and stores 15-minute
//!   energy totals and steady-state efficiency samples;
//! - **prices:** keeps day-ahead prices up to date;
//! - **weather:** fetches the forecast hourly (and from it the PV forecast),
//!   and archives past weather for training;
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
use dess_core::Slot;
use dess_core::efficiency::EfficiencySampler;
use dess_core::record::{Recorder, SlotRecord};
use dess_core::soc::SocEstimator;
use dess_core::tariff::Tariff;
use dess_core::weather::Weather;
use dess_models::heatpump::HpModel;
use dess_models::load::LoadModel;
use dess_models::pv::PvModel;
use dess_victron::probe::{ProbeReport, Severity};
use dess_victron::{Snapshot, Venus, VenusOptions, reading};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::config::{Config, LocationConfig};
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
    /// The latest weather forecast.
    pub weather: watch::Sender<Arc<BTreeMap<Slot, Weather>>>,
    pub location: watch::Sender<Option<LocationConfig>>,
    /// The learned PV model, when it beats the configured arrays.
    pub pv_model: watch::Sender<Option<Arc<PvModel>>>,
    /// The learned heat pump and base-load models, when they beat their baselines.
    pub hp_model: watch::Sender<Option<Arc<HpModel>>>,
    pub load_model: watch::Sender<Option<Arc<LoadModel>>>,
    pub status: Mutex<Status>,
    pub control: Mutex<crate::control::ControlStatus>,
    /// Bumped to make the planner run now (e.g. after an outage change).
    pub replan_now: watch::Sender<u64>,
    /// When to start the dishwasher; fixed once that time has come.
    pub cheapest_start: watch::Sender<Option<planning::CheapestStart>>,
    /// The recorder's SoC estimate (finer than the BMS's) and when it was made.
    pub soc: Mutex<Option<(Timestamp, f64)>>,
    /// The last week, replayed with dess-oxide's policy (made nightly).
    pub comparison: Mutex<Option<crate::comparison::Comparison>>,
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

    pub fn set_control(&self, status: crate::control::ControlStatus) {
        *self.control.lock().expect("control lock poisoned") = status;
    }

    pub fn control_status(&self) -> crate::control::ControlStatus {
        self.control.lock().expect("control lock poisoned").clone()
    }

    /// The SoC estimate, if it's current.
    pub fn soc(&self, now: Timestamp) -> Option<f64> {
        let estimate = *self.soc.lock().expect("soc lock poisoned");
        estimate
            .filter(|(at, _)| now.duration_since(*at) <= SignedDuration::from_secs(5))
            .map(|(_, soc)| soc)
    }
}

pub async fn run(config: Config, data_dir: &Path, listen: SocketAddr) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let shared = Arc::new(Shared::new(
        config,
        Store::open(&data_dir.join("dess.db"))?,
    )?);
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

    let tasks = spawn_tasks(&venus, &shared, &stopped)?;
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

impl Shared {
    fn new(config: Config, store: Store) -> anyhow::Result<Self> {
        let tariff = config
            .tariff
            .as_ref()
            .map(crate::config::TariffConfig::to_tariff)
            .transpose()?;
        let tz = match &tariff {
            Some(tariff) => tariff.time_zone.clone(),
            None => TimeZone::get("Europe/Amsterdam")?,
        };
        Ok(Self {
            config,
            tariff,
            tz,
            store: Mutex::new(store),
            plan: watch::Sender::new(None),
            weather: watch::Sender::new(Arc::new(BTreeMap::new())),
            location: watch::Sender::new(None),
            pv_model: watch::Sender::new(None),
            hp_model: watch::Sender::new(None),
            load_model: watch::Sender::new(None),
            status: Mutex::new(Status::default()),
            control: Mutex::new(crate::control::ControlStatus::default()),
            replan_now: watch::Sender::new(0),
            cheapest_start: watch::Sender::new(None),
            soc: Mutex::new(None),
            comparison: Mutex::new(None),
        })
    }
}

/// Starts the service's tasks; planning ones only with a tariff.
fn spawn_tasks(
    venus: &Arc<Venus>,
    shared: &Arc<Shared>,
    stopped: &watch::Receiver<bool>,
) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let mut tasks = vec![
        tokio::spawn(record(
            Arc::clone(venus),
            Arc::clone(shared),
            stopped.clone(),
        )),
        tokio::spawn(import_history(Arc::clone(shared), stopped.clone())),
    ];
    if shared.tariff.is_none() {
        info!("no [tariff] configured: recording only, no planning");
        return Ok(tasks);
    }
    let client = crate::http_client()?;
    let (prices_changed, prices_rx) = watch::channel(0u64);
    tasks.push(tokio::spawn(fetch_prices(
        Arc::clone(shared),
        client.clone(),
        prices_changed,
        stopped.clone(),
    )));
    tasks.push(tokio::spawn(fetch_weather(
        Arc::clone(shared),
        client.clone(),
        stopped.clone(),
    )));
    tasks.push(tokio::spawn(crate::entities::publish(
        Arc::clone(venus),
        Arc::clone(shared),
        client,
        stopped.clone(),
    )));
    tasks.push(tokio::spawn(train(Arc::clone(shared), stopped.clone())));
    tasks.push(tokio::spawn(crate::control::run(
        Arc::clone(venus),
        Arc::clone(shared),
        stopped.clone(),
    )));
    tasks.push(tokio::spawn(plan_loop(
        Arc::clone(venus),
        Arc::clone(shared),
        prices_rx,
        stopped.clone(),
    )));
    Ok(tasks)
}

/// Resolves once `stop` becomes true.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopping| *stopping).await;
}

async fn record(venus: Arc<Venus>, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut recorder = Recorder::new(MAX_SAMPLE_GAP);
    let mut sampler = EfficiencySampler::default();
    let mut soc = SocEstimator::new(dess_core::WattHours(0.0));
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
            // The plan's capacity includes the learned one.
            let planned = shared.plan.borrow().as_ref().map(|v| v.battery.capacity.0);
            let capacity = planned.or_else(|| {
                planning::capacity_wh(&reading::battery_info(snapshot), &shared.config, None)
            });
            let sample = reading::sample(snapshot, now).map_err(|e| e.to_string())?;
            Ok::<_, String>((sample, capacity))
        });
        match sample {
            Ok((mut sample, capacity)) => {
                soc.set_capacity(dess_core::WattHours(capacity.unwrap_or(0.0)));
                sample.soc_pct = soc.update(now, sample.soc_pct, sample.battery);
                *shared.soc.lock().expect("soc lock poisoned") = Some((now, sample.soc_pct));
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

/// Trains the learned models: shortly after startup (once the history
/// imports have had a moment), then nightly at 03:30.
async fn train(shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    // Promoted models from an earlier run are used straight away.
    let stored = crate::training::StoredModels::load(&lock(&shared.store));
    // Only announce models that exist, so a start without any doesn't replan.
    if let Some(model) = stored.pv {
        shared.pv_model.send_replace(Some(Arc::new(model)));
    }
    if let Some(model) = stored.heat_pump {
        shared.hp_model.send_replace(Some(Arc::new(model)));
    }
    if let Some(model) = stored.load {
        shared.load_model.send_replace(Some(Arc::new(model)));
    }
    let mut wait = Duration::from_secs(10 * 60);
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stopped(&mut stop) => return,
        }
        let location = *shared.location.borrow();
        if let Some(location) = location {
            train_pv(&shared, location).await;
        }
        train_house(&shared).await;
        compare_policies(&shared).await;
        wait = until_local(&shared.tz, 3, 30);
    }
}

/// Replays the last week with dess-oxide's policy, for the page.
async fn compare_policies(shared: &Arc<Shared>) {
    const DAYS: i64 = 7;
    let Some(view) = shared.plan.borrow().clone() else {
        return;
    };
    let now = Timestamp::now();
    let for_replay = Arc::clone(shared);
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let tariff = for_replay.tariff.as_ref().context("no tariff")?;
        let inputs = crate::comparison::gather(&lock(&for_replay.store), now, DAYS)?;
        anyhow::Ok(crate::comparison::compare(inputs, &view, tariff, now))
    })
    .await;
    match result {
        Ok(Ok(comparison)) => {
            info!(
                hours = comparison.hours,
                actual_eur = format!("{:.2}", comparison.actual),
                replayed_eur = format!("{:.2}", comparison.replayed),
                perfect_eur = format!("{:.2}", comparison.perfect),
                without_battery_eur = format!("{:.2}", comparison.without_battery),
                seconds = format!("{:.1}", started.elapsed().as_secs_f64()),
                "replayed the last week"
            );
            *shared.comparison.lock().expect("comparison lock poisoned") = Some(comparison);
        }
        Ok(Err(error)) => warn!("replaying the last week: {error:#}"),
        Err(error) => error!(%error, "the replay panicked"),
    }
}

async fn train_pv(shared: &Arc<Shared>, location: LocationConfig) {
    let now = Timestamp::now();
    let shared_for_fit = Arc::clone(shared);
    let result = tokio::task::spawn_blocking(move || {
        let store = lock(&shared_for_fit.store);
        crate::training::train_pv(&store, &shared_for_fit.config, location, now)
    })
    .await;
    match result {
        Ok(Ok(Some(report))) => {
            let promoted = report.improves();
            info!(
                hours = report.hours,
                learned_mae_kwh = format!("{:.3}", report.validation_mae),
                configured_mae_kwh = format!("{:.3}", report.initial_validation_mae),
                promoted,
                model = %crate::training::pv_model_json(&report.model),
                "trained the PV model"
            );
            let saved = lock(&shared.store).save_model(
                "pv",
                now,
                &crate::training::pv_model_json(&report.model),
                &crate::training::fit_metrics(&report),
                promoted,
            );
            if let Err(error) = saved {
                error!(%error, "storing the PV model");
            }
            shared
                .pv_model
                .send_replace(promoted.then(|| Arc::new(report.model)));
        }
        Ok(Ok(None)) => info!("not enough history to train the PV model yet"),
        Ok(Err(error)) => warn!("training the PV model: {error:#}"),
        Err(error) => error!(%error, "PV training panicked"),
    }
}

async fn train_house(shared: &Arc<Shared>) {
    let now = Timestamp::now();
    let for_fit = Arc::clone(shared);
    let result = tokio::task::spawn_blocking(move || {
        let store = lock(&for_fit.store);
        crate::training::train_house(&store, &for_fit.config, &for_fit.tz, now)
    })
    .await;
    let (heat_pump, load) = match result {
        Ok(Ok(fits)) => fits,
        Ok(Err(error)) => return warn!("training the house models: {error:#}"),
        Err(error) => return error!(%error, "house training panicked"),
    };
    let store = lock(&shared.store);
    let heat_pump = heat_pump.map(|fit| Fitted {
        params: crate::training::hp_model_json(&fit.model),
        hours: fit.hours,
        validation_mae: fit.validation_mae,
        baseline_mae: fit.baseline_mae,
        promoted: fit.improves(),
        model: fit.model,
    });
    publish_fit(
        &store,
        now,
        "heat_pump",
        "heat pump",
        heat_pump,
        &shared.hp_model,
    );
    let load = load.map(|fit| Fitted {
        params: crate::training::load_model_json(&fit.model),
        hours: fit.hours,
        validation_mae: fit.validation_mae,
        baseline_mae: fit.baseline_mae,
        promoted: fit.improves(),
        model: fit.model,
    });
    publish_fit(&store, now, "load", "base load", load, &shared.load_model);
}

/// A fitted model with a baseline to beat.
struct Fitted<M> {
    model: M,
    params: serde_json::Value,
    hours: usize,
    validation_mae: f64,
    baseline_mae: f64,
    promoted: bool,
}

/// Logs, stores and (when it beats its baseline) publishes a fitted model.
fn publish_fit<M>(
    store: &Store,
    now: Timestamp,
    name: &str,
    label: &str,
    fit: Option<Fitted<M>>,
    sender: &watch::Sender<Option<Arc<M>>>,
) {
    let Some(fit) = fit else {
        info!("not enough {label} history to train yet");
        return;
    };
    info!(
        hours = fit.hours,
        learned_mae_kwh = format!("{:.3}", fit.validation_mae),
        baseline_mae_kwh = format!("{:.3}", fit.baseline_mae),
        promoted = fit.promoted,
        "trained the {label} model"
    );
    let metrics =
        crate::training::baseline_metrics(fit.hours, fit.validation_mae, fit.baseline_mae);
    if let Err(error) = store.save_model(name, now, &fit.params, &metrics, fit.promoted) {
        error!(%error, "storing the {label} model");
    }
    sender.send_replace(fit.promoted.then(|| Arc::new(fit.model)));
}

/// Time until the next `hour:minute` local time.
fn until_local(tz: &TimeZone, hour: i8, minute: i8) -> Duration {
    let now = Timestamp::now();
    let today = now.to_zoned(tz.clone()).date();
    let next = [today, today.tomorrow().unwrap_or(today)]
        .into_iter()
        .filter_map(|d| d.at(hour, minute, 0, 0).to_zoned(tz.clone()).ok())
        .map(|z| z.timestamp())
        .find(|t| *t > now)
        .unwrap_or(now + SignedDuration::from_hours(24));
    next.duration_since(now).unsigned_abs()
}

/// Hourly: the weather forecast, and from it the PV forecast. Once a day's
/// archive is complete, it's copied too, for training.
async fn fetch_weather(
    shared: Arc<Shared>,
    client: reqwest::Client,
    mut stop: watch::Receiver<bool>,
) {
    let mut location = None;
    loop {
        if location.is_none() {
            match planning::resolve_location(&client, &shared.config).await {
                Ok(found) => {
                    location = Some(found);
                    shared.location.send_replace(Some(found));
                }
                Err(error) => warn!("no weather yet: {error:#}"),
            }
        }
        if let Some(location) = location {
            let now = Timestamp::now();
            match crate::openmeteo::forecast(&client, location).await {
                Ok(weather) => {
                    if let Err(error) = tokio::task::block_in_place(|| {
                        lock(&shared.store).save_weather(&weather, false, now)
                    }) {
                        error!(%error, "storing the weather forecast");
                    }
                    shared.update_status(|s| s.pv_updated = Some(now));
                    shared.weather.send_replace(Arc::new(weather));
                }
                Err(error) => warn!("weather forecast: {error:#}"),
            }
            if let Err(error) = archive_weather(&shared, &client, location, now).await {
                warn!("weather archive: {error:#}");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
            () = stopped(&mut stop) => return,
        }
    }
}

/// Copies the historical-forecast archive up to the day before yesterday
/// (the archive lags a little), 90 days per request.
async fn archive_weather(
    shared: &Shared,
    client: &reqwest::Client,
    location: crate::config::LocationConfig,
    now: Timestamp,
) -> anyhow::Result<()> {
    let tz = &shared.tz;
    let last_day = now.to_zoned(tz.clone()).date().yesterday()?.yesterday()?;
    let mut start = match lock(&shared.store).last_weather_history()? {
        Some(slot) => slot.start().to_zoned(tz.clone()).date().tomorrow()?,
        None => crate::openmeteo::HISTORY_START,
    };
    while start <= last_day {
        let end = start.checked_add(jiff::ToSpan::days(89))?.min(last_day);
        let weather = crate::openmeteo::history(client, location, start, end).await?;
        tokio::task::block_in_place(|| lock(&shared.store).save_weather(&weather, true, now))?;
        info!(from = %start, to = %end, slots = weather.len(), "archived weather");
        start = end.tomorrow()?;
    }
    Ok(())
}

async fn plan_loop(
    venus: Arc<Venus>,
    shared: Arc<Shared>,
    mut prices: watch::Receiver<u64>,
    mut stop: watch::Receiver<bool>,
) {
    let mut weather = shared.weather.subscribe();
    let mut model = shared.pv_model.subscribe();
    let mut hp_model = shared.hp_model.subscribe();
    let mut load_model = shared.load_model.subscribe();
    let mut replan_now = shared.replan_now.subscribe();
    loop {
        replan(&venus, &shared);
        let now = Timestamp::now();
        let next = Slot::containing(now).end() + SignedDuration::from_secs(5);
        let changed = tokio::select! {
            () = tokio::time::sleep(next.duration_since(now).unsigned_abs()) => false,
            _ = prices.changed() => true,
            _ = weather.changed() => true,
            _ = model.changed() => true,
            _ = hp_model.changed() => true,
            _ = load_model.changed() => true,
            _ = replan_now.changed() => true,
            () = stopped(&mut stop) => return,
        };
        if changed {
            // Changes come in bursts (prices, weather and models at startup):
            // give the rest a moment, so one replan covers them all.
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
                () = stopped(&mut stop) => return,
            }
            prices.mark_unchanged();
            weather.mark_unchanged();
            model.mark_unchanged();
            hp_model.mark_unchanged();
            load_model.mark_unchanged();
            replan_now.mark_unchanged();
        }
    }
}

fn replan(venus: &Venus, shared: &Shared) {
    let Some(tariff) = &shared.tariff else { return };
    let now = Timestamp::now();
    let snapshot = venus.snapshot();
    let weather = shared.weather.borrow().clone();
    let (pv_model, hp_model, load_model) = (
        shared.pv_model.borrow().clone(),
        shared.hp_model.borrow().clone(),
        shared.load_model.borrow().clone(),
    );
    let pv = match *shared.location.borrow() {
        Some(location) => {
            planning::pv_from_weather(&weather, &shared.config, location, pv_model.as_deref())
        }
        None => BTreeMap::new(),
    };
    let outage = planning::outage_window(&lock(&shared.store), now);
    let inputs = planning::ForecastInputs {
        pv: &pv,
        weather: &weather,
        models: planning::Models {
            heat_pump: hp_model.as_deref(),
            load: load_model.as_deref(),
        },
        outage,
        soc: shared.soc(now),
    };
    let result = fresh(venus, &snapshot, now)
        .map_err(anyhow::Error::msg)
        .and_then(|()| {
            tokio::task::block_in_place(|| {
                let mut store = lock(&shared.store);
                let view =
                    planning::make_plan(now, &snapshot, &store, &shared.config, tariff, inputs)?;
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
            let next =
                planning::cheapest_start(&view, &shared.config.cheapest_start, &shared.tz, now);
            shared.cheapest_start.send_if_modified(|current| {
                // Once the start time has come, it stays until the window closes.
                let started = current.is_some_and(|c| c.start <= now && now < c.window_end);
                let changed = !started && *current != next;
                if changed {
                    *current = next;
                }
                changed
            });
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
            if error.is::<planning::NoPrices>() {
                info!("waiting for day-ahead prices before planning");
                return;
            }
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
