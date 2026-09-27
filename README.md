# dess-oxide

A Rust replacement for the [Day Ahead Optimizer](https://github.com/corneel27/day-ahead)
for Victron ESS systems.

It plans the battery against Dutch 15-minute day-ahead prices, using
forecasts of PV, house load and heat pump demand that it learns from the
system's own history. It talks to the Cerbo GX directly over the Cerbo's
local MQTT, and ships as a Home Assistant app. The full design is in
[docs/PLAN.md](docs/PLAN.md).

**Status: M1, shadow mode.** It records, fetches prices, forecasts and plans, and shows it all on its own page. It never writes to the Victron yet.

## Layout

| Path | What |
|---|---|
| `crates/dess-core` | Pure domain logic: slots, units, recording, efficiency sampling, tariff, price horizon, baseline forecasts, the DP planner |
| `crates/dess-victron` | Read-only client for the GX device's MQTT, typed readings, `probe` report |
| `crates/dess-oxide` | The binary: config, SQLite store, Nord Pool and Open-Meteo clients, the service, the web page |
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

Run the service in shadow mode: it records, plans every quarter hour, and
serves its page on http://127.0.0.1:8099. Also read-only.

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
   checks, builds and publishes the images.
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
