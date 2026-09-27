# dess-oxide

A Rust replacement for the [Day Ahead Optimizer](https://github.com/corneel27/day-ahead)
for Victron ESS systems.

It plans the battery against Dutch 15-minute day-ahead prices, using
forecasts of PV, house load and heat pump demand that it learns from the
system's own history. It talks to the Cerbo GX directly over the Cerbo's
local MQTT, and ships as a Home Assistant app. The full design is in
[docs/PLAN.md](docs/PLAN.md).

**Status: M0.** For now it records, read-only.

## Layout

| Path | What |
|---|---|
| `crates/dess-core` | Pure domain logic: 15-minute slots, units, energy recording, efficiency sampling |
| `crates/dess-victron` | Read-only client for the GX device's MQTT, typed readings, `probe` report |
| `crates/dess-oxide` | The binary: config, `SQLite` store, `probe` and `run` commands |
| `dess_oxide/` | The Home Assistant app definition |

## Usage

Print a report on a GX device's configuration. This is read-only: it only
sends MQTT read requests.

```bash
cargo run -- probe --host 192.168.1.20
```

Record energy flows into `data/dess.db`. Also read-only.

```bash
cp dess.example.toml dess.toml   # then set the host
cargo run -- run --config dess.toml --data-dir data
```

## Home Assistant

1. Add this repository in Settings → Apps → App store → Repositories.
2. Install **dess-oxide**.
3. Set the Cerbo's address in the app's configuration.

Images are built by `.github/workflows/release.yml` when a `v<version>` tag
is pushed. The tag must match `version` in `dess_oxide/config.yaml`.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```
