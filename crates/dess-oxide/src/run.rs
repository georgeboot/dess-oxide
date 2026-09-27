//! The service loop. For now it only records: once a second it samples the
//! system and stores 15-minute energy totals and steady-state conversion
//! samples. It never writes to the GX device.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use dess_core::efficiency::EfficiencySampler;
use dess_core::record::{Recorder, SlotRecord};
use dess_victron::probe::{ProbeReport, Severity};
use dess_victron::{Venus, VenusOptions, reading};
use jiff::{SignedDuration, Timestamp};
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::store::Store;

/// Samples further apart than this are not integrated.
const MAX_SAMPLE_GAP: SignedDuration = SignedDuration::from_secs(10);
/// Without a heartbeat for this long, the snapshot is considered stale.
const MAX_HEARTBEAT_AGE: SignedDuration = SignedDuration::from_secs(15);

pub async fn run(config: Config, data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let mut store = Store::open(&data_dir.join("dess.db"))?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let options = VenusOptions {
        host: config.victron.host.clone(),
        port: config.victron.port,
        portal_id: config.victron.portal_id.clone(),
    };
    let venus = tokio::select! {
        venus = connect_with_retry(options) => venus,
        () = &mut shutdown => return Ok(()),
    };
    if let Err(error) = venus.full_publish(Duration::from_secs(30)).await {
        warn!(%error, "continuing without a complete first snapshot");
    }
    log_findings(&venus);
    wait_for_heartbeat(&venus, Duration::from_secs(10)).await;

    let mut recorder = Recorder::new(MAX_SAMPLE_GAP);
    let mut sampler = EfficiencySampler::default();
    let mut warnings = RateLimitedWarning::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!("recording (read-only)");
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = &mut shutdown => break,
        }
        let now = Timestamp::now();
        let sample = venus.with_snapshot(|snapshot| {
            let heartbeat_age = snapshot.last_heartbeat().map(|at| now.duration_since(at));
            if !venus.is_connected() || heartbeat_age.is_none_or(|age| age > MAX_HEARTBEAT_AGE) {
                return Err("no recent heartbeat from the GX device".to_owned());
            }
            reading::sample(snapshot, now).map_err(|e| e.to_string())
        });
        match sample {
            Ok(sample) => {
                warnings.clear();
                sampler.push(
                    now,
                    sample.inverter_ac,
                    sample.battery - sample.pv_dc,
                    sample.battery_voltage,
                );
                for record in recorder.push(sample) {
                    persist(&mut store, &record, &mut sampler, now);
                }
            }
            Err(message) => warnings.warn(&message),
        }
    }

    info!("shutting down");
    if let Some(record) = recorder.flush() {
        persist(&mut store, &record, &mut sampler, Timestamp::now());
    }
    venus.close().await;
    Ok(())
}

fn persist(
    store: &mut Store,
    record: &SlotRecord,
    sampler: &mut EfficiencySampler,
    now: Timestamp,
) {
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
        store.save_slot(record, now.as_second())?;
        store.save_efficiency_bins(day, &sampler.take_bins())
    });
    if let Err(error) = result {
        error!(%error, slot = %record.slot, "failed to store slot");
    }
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

fn log_findings(venus: &Venus) {
    let report = venus.with_snapshot(|snapshot| {
        ProbeReport::from_snapshot(venus.portal_id(), snapshot, 0.0, Timestamp::now())
    });
    for finding in &report.findings {
        match finding.severity {
            Severity::Info => info!("{}", finding.message),
            Severity::Warning | Severity::Blocker => warn!("{}", finding.message),
        }
    }
}

/// Logs a warning when it changes, and repeats it at most once a minute.
#[derive(Default)]
struct RateLimitedWarning {
    last: Option<(String, std::time::Instant)>,
}

impl RateLimitedWarning {
    fn warn(&mut self, message: &str) {
        let repeat = self
            .last
            .as_ref()
            .is_some_and(|(last, at)| last == message && at.elapsed() < Duration::from_secs(60));
        if !repeat {
            warn!("{message}; not recording");
            self.last = Some((message.to_owned(), std::time::Instant::now()));
        }
    }

    fn clear(&mut self) {
        if self.last.take().is_some() {
            info!("recording again");
        }
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
