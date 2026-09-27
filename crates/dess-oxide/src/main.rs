mod chart;
mod config;
mod control;
mod homeassistant;
mod nordpool;
mod openmeteo;
mod planning;
mod run;
mod store;
mod training;
mod web;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};
use dess_victron::probe::ProbeReport;
use dess_victron::{Venus, VenusOptions};
use jiff::Timestamp;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a read-only report on a GX device's configuration.
    Probe {
        /// Host name or IP address of the GX device.
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 1883)]
        port: u16,
        /// VRM portal id; discovered when not given.
        #[arg(long)]
        portal_id: Option<String>,
        /// How long to watch for changing values after the full publish.
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print the plan the optimiser would follow right now. Read-only.
    Plan {
        /// `options.json` (Home Assistant app) or a TOML file.
        #[arg(long, default_value = "dess.toml")]
        config: PathBuf,
        /// Where the database lives (prices and recorded history).
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
        /// How many slots to print.
        #[arg(long, default_value_t = 32)]
        rows: usize,
    },
    /// Run the service: record the system's energy flows. Read-only for now.
    Run {
        /// `options.json` (Home Assistant app) or a TOML file.
        #[arg(long, default_value = "dess.toml")]
        config: PathBuf,
        /// Where the database lives.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
        /// Address for the web page (Home Assistant ingress uses port 8099).
        #[arg(long, default_value = "127.0.0.1:8099")]
        listen: std::net::SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,rumqttc=warn")),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();

    // ring as the TLS crypto provider (see the rustls dependency).
    let _ = rustls::crypto::ring::default_provider().install_default();

    match Cli::parse().command {
        Command::Probe {
            host,
            port,
            portal_id,
            seconds,
            json,
        } => {
            probe(
                VenusOptions {
                    host,
                    port,
                    portal_id,
                },
                Duration::from_secs(seconds),
                json,
            )
            .await
        }
        Command::Plan {
            config,
            data_dir,
            rows,
        } => plan(Config::load(&config)?, &data_dir, rows).await,
        Command::Run {
            config,
            data_dir,
            listen,
        } => run::run(Config::load(&config)?, &data_dir, listen).await,
    }
}

async fn probe(options: VenusOptions, watch: Duration, json: bool) -> anyhow::Result<()> {
    let started = Instant::now();
    let venus = Venus::connect(options, Duration::from_secs(15)).await?;
    venus.full_publish(Duration::from_secs(30)).await?;
    tokio::time::sleep(watch).await;
    let report = venus.with_snapshot(|snapshot| {
        ProbeReport::from_snapshot(
            venus.portal_id(),
            snapshot,
            started.elapsed().as_secs_f64(),
            Timestamp::now(),
        )
    });
    venus.close().await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{report}");
    }
    Ok(())
}

/// The HTTP client for prices and weather.
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!(
            "dess-oxide/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/georgeboot/dess-oxide)"
        ))
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn plan(config: Config, data_dir: &std::path::Path, rows: usize) -> anyhow::Result<()> {
    let tariff = config
        .tariff
        .as_ref()
        .context("planning needs a [tariff] section")?
        .to_tariff()?;
    std::fs::create_dir_all(data_dir)?;
    let store = std::sync::Mutex::new(store::Store::open(&data_dir.join("dess.db"))?);
    let client = http_client()?;
    let now = Timestamp::now();

    let nordpool = nordpool::NordPool::new(client.clone(), &config.prices.area);
    planning::update_prices(&store, &nordpool, now, &tariff.time_zone).await?;
    // The models the service trained, when they're in use.
    let models = training::StoredModels::load(&planning::lock(&store));
    let (weather, pv) = match planning::resolve_location(&client, &config).await {
        Ok(location) => {
            let weather = openmeteo::forecast(&client, location).await?;
            let pv = planning::pv_from_weather(&weather, &config, location, models.pv.as_ref());
            (weather, pv)
        }
        Err(error) => {
            tracing::warn!("no weather: {error:#}");
            Default::default()
        }
    };

    let venus = Venus::connect(
        VenusOptions {
            host: config.victron.host.clone(),
            port: config.victron.port,
            portal_id: config.victron.portal_id.clone(),
        },
        Duration::from_secs(15),
    )
    .await?;
    venus.full_publish(Duration::from_secs(30)).await?;
    let started = Instant::now();
    let view = venus.with_snapshot(|snapshot| {
        planning::make_plan(
            now,
            snapshot,
            &planning::lock(&store),
            &config,
            &tariff,
            planning::ForecastInputs {
                pv: &pv,
                weather: &weather,
                models: models.as_models(),
            },
        )
    })?;
    let elapsed = started.elapsed();
    venus.close().await;

    println!(
        "SoC {:.0} %, capacity {:.1} kWh, charge ≤ {:.1} kW, discharge ≤ {:.1} kW; planned in {elapsed:?}\n",
        view.soc,
        view.battery.capacity.0 / 1000.0,
        view.battery.max_charge_ac.0 / 1000.0,
        view.battery.max_discharge_ac.0 / 1000.0,
    );
    print!(
        "{}",
        planning::render(&view.plan, &view.forecasts, &tariff.time_zone, rows)
    );
    Ok(())
}
