mod config;
mod run;
mod store;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    /// Run the service: record the system's energy flows. Read-only for now.
    Run {
        /// `options.json` (Home Assistant app) or a TOML file.
        #[arg(long, default_value = "dess.toml")]
        config: PathBuf,
        /// Where the database lives.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
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
        Command::Run { config, data_dir } => run::run(Config::load(&config)?, &data_dir).await,
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
