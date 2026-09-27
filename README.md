# dess-oxide

A Rust replacement for the [Day Ahead Optimizer](https://github.com/corneel27/day-ahead)
for Victron ESS systems.

It plans the battery against Dutch 15-minute day-ahead prices, using
forecasts of PV, house load and heat pump demand that it learns from the
system's own history, and forecasts the prices that aren't published yet.
It talks to the Cerbo GX directly over the Cerbo's local MQTT, and ships as
a Home Assistant app. How it works is in [docs/DESIGN.md](docs/DESIGN.md);
installing and configuring it in [dess_oxide/DOCS.md](dess_oxide/DOCS.md).

**Status: complete, running as a dry run next to DAO until the handover.**
It records, learns its models (PV, heat pump, hot water, base load, prices,
the battery's losses, capacity and round trip), plans, and shows it all on
its own page, including a nightly replay of the last week against what
actually happened. Control (a one-second setpoint loop, bypass, the PV
relay, outage preparation, manual overrides) is built. It writes only when
both locks are on: `dryrun: false` in the options and the switch on its
page. What's left is listed at the end of the design.

## Layout

| Path | What |
|---|---|
| `crates/dess-core` | Pure domain logic: slots, units, recording, efficiency, capacity and cell round-trip learning, SoC estimation, tariff, price horizon, weather correction, baseline forecasts, the DP planner, the per-second policy, the replay |
| `crates/dess-models` | Learned models: PV, heat pump and base load (burn), hot water (statistics), prices (gradient-boosted trees) |
| `crates/dess-victron` | Client for the GX device's MQTT: typed readings, the `probe` report, and a separate write capability |
| `crates/dess-oxide` | The binary: config, SQLite store, the Nord Pool, Open-Meteo, EnergyZero, NED and Home Assistant clients, the service, control, the web page |
| `dess_oxide/` | The Home Assistant app definition |

## Usage

Print a report on a GX device's configuration. This is read-only: it only
sends MQTT read requests.

```bash
cargo run -- probe --host 192.168.1.20
```

Print the plan the optimiser would follow right now. Read-only.

```bash
cp dess.example.toml dess.toml   # then set the host, tariff, location and PV arrays
cargo run -- plan --config dess.toml --data-dir data
```

Run the service: it records, plans every quarter hour, and serves its page
on http://127.0.0.1:8099. Read-only unless the config has `dryrun = false`
and the page's switch is on.

```bash
cargo run -- run --config dess.toml --data-dir data
```

## Home Assistant

1. Add this repository in Settings → Apps → App store → Repositories.
2. Install **dess-oxide**.
3. Set the Cerbo's address in the app's configuration.

## Releasing

1. Bump `version` in `Cargo.toml` and `dess_oxide/config.yaml`, and add a
   `dess_oxide/CHANGELOG.md` entry.
2. Commit, tag `v<version>`, and push **only the tag**. The release workflow
   checks, builds and publishes the images, then creates the GitHub release
   with the changelog section as notes (`scripts/release-notes.sh`).
3. Push `main` after the release succeeds.

Home Assistant reads the app definition from `main`, so this order means it
never offers an update whose image doesn't exist yet. Never change the
options in `dess_oxide/config.yaml` without a version bump.

## Development

```bash
cargo fmt --all
scripts/check.sh   # exactly what CI and releases check: fmt, clippy, tests
```

The toolchain is pinned in `rust-toolchain.toml`; bump it together with the
Dockerfile's `rust` image.
