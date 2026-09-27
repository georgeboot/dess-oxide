# dess-oxide: design and plan

Status (2026-09-27): M0 released as v0.1.0 and running at George's site. M1 mostly built: prices, tariff, the DP planner in shadow mode, baseline forecasts and the dess-oxide page.

dess-oxide runs a Victron ESS against Dutch 15-minute day-ahead prices. Every quarter hour it:

1. forecasts PV, house load and heat pump demand,
2. computes the cheapest battery schedule over the known price horizon,
3. executes it directly on the Cerbo GX over the Cerbo's local MQTT, with a one-second control loop.

It ships as a Home Assistant app (formerly "add-on"). Its UI is its own page in the HA sidebar (ingress): the plan, forecasts, accuracy and controls. It creates no HA entities unless asked to (§17), and HA is not in the control path.

It replaces the Day Ahead Optimizer ([DAO](https://github.com/corneel27/day-ahead)) and the HA helper and automation chain that currently executes DAO's output.

---

## 1. Goals

- **Minimise the electricity bill.** That is import cost minus export revenue, plus battery wear, for a three-phase Victron ESS with AC-coupled PV, a heat pump and, at one site, an EV.
- **Talk to the Victron system directly** over the Cerbo's local MQTT. HA hosts the UI page and provides optional extra sensors (weather station, heat pump meter).
- **Need almost no configuration.** Anything that can be measured is learned.
- **Be safe to leave running.** Crashes, stale data and missing prices must lead to bounded, predictable behaviour.
- **Be provably better.** Every change to a model or the planner is judged by a backtest on our own history.

## 2. The two sites

One code path serves both sites; everything site-specific lives in config.

| | George | Brother |
|---|---|---|
| Inverter/chargers | 3× MultiPlus-II 48/5000, three-phase ESS, Cerbo GX on Venus OS 3.66 | same |
| Battery | ≈ 32 kWh, 16S LFP, 628 Ah, JK-BMS on CAN via DVCC (whole-% SoC). DAO config caps charge at 10 kW and discharge at 11 kW | tbd |
| PV | AC-coupled on AC-out, one inverter, visible in Venus. 5.59 kWp at 33° tilt and 3.44 kWp at 9° tilt, both facing azimuth 193° (13° W of S) | AC-coupled, tbd |
| PV curtailment | contactor on Cerbo relay 2 (de-energised = PV on) | tbd |
| Grid | 3×25 A, 17 kW. No Victron grid meter: the MultiPlus AC-in is the measurement. HomeWizard P1 meter in HA | separate grid meter connected to the Victrons |
| Heat pump | Itho Daalderop Amber running [OpenAmber](https://github.com/Jordi1990/openamber), does DHW too. On AC-out (backed up). HomeWizard kWh meter | tbd |
| EV | none yet | charger on the grid side of the Victrons: counted by the Victron grid meter, not backed up |
| Weather station | Ecowitt WS90 in HA | tbd |

## 3. Principles

1. **Learn, don't configure.** If it can be measured, it's learned from history. That covers:
   - conversion efficiency curves and idle losses
   - usable capacity and SoC-dependent power limits
   - PV orientation, yield and shading
   - base load and heat pump demand

   Config holds only what can't be measured: tariffs, the relay number, HA entity ids, grid limits. PV watt-peak and orientation are optional starting points. DAO's charge/discharge stage tables, `yield_factor`s, 24-value `baseload` and end-SoC helpers all disappear.
2. **One hop to the hardware.** We talk to the Cerbo over its local MQTT and read back what we write. There is no chain of HA helpers, automations and integrations in between.
3. **Victron stays in charge of safety.** We only move the grid setpoint (a volatile override) and the PV relay. ESS keeps enforcing BMS limits, minimum SoC and phase balance. Every failure we can detect releases control back to plain ESS (§15.3).
4. **15-minute native, rolling horizon.** We replan every quarter hour, and also on events: new prices, a forecast miss, the outage toggle. Each replan starts from the measured SoC.
5. **Pure core, I/O at the edges.** Forecasters, tariff and planner are deterministic functions of their inputs, with no async code and no clock. That makes them unit-testable, and the same code runs in backtests over months of history.
6. **Measure ourselves.** Forecast accuracy and realised savings against baselines are published as sensors. A model is used only while it beats its baseline.
7. **Small surface.** The tool supports Victron only, and NL / Nord Pool first. It has no appliance programs, reports, graph renderer, notification framework or config GUI; HA already does those.

## 4. What we do differently from DAO

Based on a read of DAO at commit `b4933d5` (2026-09-18, app version 2026.9.1) and George's DAO config.

| Area | DAO | dess-oxide |
|---|---|---|
| Control path | Writes HA helpers for interval 0 only, once per run (`input_number.feedin_grid`, `input_select.ess_operation_modus`, …). User automations and a Victron integration carry the values to the Cerbo. Setpoints are open-loop. If a run fails, the last values stay in place indefinitely. All actuator writes share one `try/except`, so one failing device skips the rest. | Straight to the Cerbo over its local MQTT, verified by readback. A 1 Hz loop applies the planner's policy to measured load and PV and writes the volatile setpoint override, where DAO's chain writes the persisted setting. Control is released to plain ESS on any detected failure (§15). |
| Battery efficiency | Hand-entered charge/discharge "stages" (SOS2) plus two DC constants. Idle losses are not modelled. | Learned loss curve per direction (idle + linear + quadratic), plus usable capacity and charge taper, refreshed nightly (§12.2). |
| PV forecast | Isotropic transposition with a hand-tuned `yield` in kWh per J/cm², and no temperature. Optional XGBoost is trained on KNMI station observations in J/cm², then fed Meteoserver forecasts in W/m² (DAO issue #835, a 5–8× error). | Physics model with tilt, azimuth, kWp, temperature coefficient, shading map and clipping all learned. It trains on the same forecast source it predicts from. Config values are optional (§12.3). |
| House load | 24 hourly values, or a per-weekday-hour mean over N days. No temperature or holiday effects. | 15-minute resolution, heat pump and EV separated out, temperature- and holiday-aware, with quantiles. It must beat a seasonal-naive baseline (§12.4). |
| Heat pump | Not modelled in George's setup (`heater_present: false`). DAO's heating module uses weighted degree-days with fixed COPs and lets heat shift freely in time. | Forecast only, no control. Temperature with a learned building lag, humidity-driven defrost losses, wind and solar gains, COP as a function of outdoor temperature, and DHW timing from OpenAmber's schedule (§12.5). |
| Optimiser | python-mip/CBC MILP with a €0.005 gap and a 1,500-node cap. Nondeterministic when multithreaded. Built, solved, dispatched and plotted in one 4,989-line function. | Exact dynamic programming over SoC. Milliseconds per solve, deterministic, handles arbitrary nonlinear efficiency, and needs no native solver (§14). |
| End of horizon | Manual `min/max_soc_einde_opt` helpers. Leftover energy is valued at the horizon's average price. | The horizon is extended with an estimated price tail, and the value of stored energy follows from that. No knobs. |
| Scheduling | 25 hand-written cron entries: 5 fixed price-retry times, calc at `xx00/15/30/45`, and so on. Tasks run serially, and missed minutes are skipped. | Event-driven: replan at every slot boundary and on events. Fetches run until the data is available, with backoff. |
| HA data | Reads HA's SQLite recorder file (`home-assistant_v2.db`) directly. | HA WebSocket API only (statistics and live states). |
| Outage | Nothing. | First-class feature: reserve for a planned window, PV forced on while islanded, grid-loss detection (§16). |
| Config | About 220 fields, keys with spaces, W and kW mixed, three schema versions with migrations. | About 30 lines, one unit convention, validated at startup (§8). |
| Web UI access | Its own port on the LAN, next to HA ingress. | Ingress only: accepts connections from the Supervisor alone, with no exposed port. |
| Tests | About 90 tests, all for config loading. None for the optimiser, PV, prices or tariffs. CI doesn't run pytest. | Unit and property tests for the core, a fake Cerbo, and regression backtests in CI (§18). |

What DAO gets right, and we keep:
- date-effective tariff components
- a receding horizon that executes only the first slot
- power-dependent conversion efficiency (we learn it instead of asking for it)
- the load identity `load = import − export + pv − battery_in + battery_out`

## 5. Scope

**v1 does:**
- Victron MQTT client and executor, including PV-relay curtailment
- Nord Pool 15-minute prices with ENTSO-E as fallback
- a date-effective tariff model that handles the end of net metering (salderen) on 2027-01-01
- weather forecasts (KNMI Harmonie via Open-Meteo), with optional WS90 observations
- forecasters for PV, base load and heat pump, plus battery and inverter identification
- a DP planner with PV curtailment and outage reserve
- the dess-oxide page (HA ingress) for plan, forecasts and controls; the HA WebSocket for input sensors and history bootstrap; optional MQTT entities
- shadow mode, a backtester, and the `probe` CLI

**Later, maybe:**
- EV smart charging (brother)
- probabilistic planning
- better price forecasting beyond D+1
- the HomeWizard local API
- automatic outage triggers from KNMI weather warnings

**Out of scope:**
- heat pump control
- appliance programs; replaced by one "cheapest start" sensor (§14.4)
- reports and graphs; HA's energy dashboard covers those
- batteries from other vendors

## 6. Architecture

```
                 ┌──────────────────────── dess-oxide (one process, tokio) ────────────────────────┐
  Cerbo GX  ◀───▶│ victron::client (1 s) ──► recorder ──► SQLite (/data/dess.db)                     │
 (local MQTT)    │ victron::executor (1 Hz) ◀── value function + measured load/PV                    │
                 │                                                                                   │
  Nord Pool ────▶│ prices (from 12:55 CET, until complete) ─┐                                        │
  Open-Meteo ───▶│ weather (hourly)                         ├──▶ planner (every slot + on events)    │
  HA websocket ─▶│ ha::inputs (WS90, HP meter, OpenAmber)   ┘    forecasts → tariff → DP → plan     │
  MQTT broker ◀─▶│ ha::entities (discovery, states, commands) ◀── plan, status, accuracy             │
  HA ingress ◀───│ web (axum): plan page + JSON                                                      │
                 │ trainer (nightly): refit models from SQLite                                       │
                 └───────────────────────────────────────────────────────────────────────────────────┘
```

Tasks communicate through `tokio::sync::watch` for latest-value state (measurements, prices, forecasts, plan) and `mpsc` for commands from HA. There is no shared mutable state. Every task restarts with backoff on error. A task failing degrades the `status` sensor, never the process.

Timing:
- **Victron client:** receives about 30 values by MQTT push, roughly once a second. It integrates energy per 15-minute slot and feeds steady-state samples to the efficiency identifier.
- **Slot boundary** (xx:00/15/30/45 plus a few seconds): close the slot record, run the planner, publish, and let the executor apply.
- **Extra replans:** new prices, a new weather run, outage or override toggles, SoC more than 3 % off plan, a PV or load miss over a threshold.
- **Executor:** a 1 Hz loop that turns the planner's value function plus measured load and PV into a grid setpoint (§15.2).
- **Prices:** from 12:55 CET, poll every 5 minutes until D+1 is complete (the API returns HTTP 204 until then). If prices are still missing by 14:00, try ENTSO-E and raise `status`.
- **Weather:** hourly, since KNMI Harmonie runs every hour.
- **Trainer:** nightly, and on startup when models are stale.

## 7. Repository layout

```
dess-oxide/
├── Cargo.toml              # workspace
├── crates/
│   ├── dess-core/          # pure domain: slots and units, tariff, DP planner, outage reserve. No tokio, no I/O.
│   ├── dess-models/        # forecasting and system identification (burn, ndarray backend) plus baselines
│   ├── dess-victron/       # Venus MQTT client: typed D-Bus paths, keepalive, value freshness, fake Cerbo for tests
│   └── dess-oxide/         # binary: config, tasks, price/weather clients, HA WebSocket + MQTT, SQLite, web, CLI
├── dess_oxide/             # the HA app: config.yaml, Dockerfile, DOCS.md, icon, translations
├── repository.yaml         # makes this git repo addable as an HA app repository
├── docs/
└── .github/workflows/
```

`dess-models` is separate because burn is heavy to compile. `dess-core`, which holds the planner, stays quick to build and test and doesn't depend on burn.

CLI subcommands of the one binary:

| Command | Purpose |
|---|---|
| `run` | The service. The app's default. |
| `probe --host <cerbo>` | Read-only snapshot of a Cerbo: firmware, ESS and DVCC settings, relays, services, leftover DESS slots, and every value we use. Built first; useful at both sites. |
| `import` | Backfill history: HA statistics, past weather forecasts, older prices (§11). |
| `train` | Fit all models and print diagnostics: efficiency curve, PV parameters, accuracy vs baseline. |
| `backtest --from --to` | Replay history through forecasters and planner; report cost vs baselines (§18). |
| `plan --at <time>` | Dry-run the planner for a past moment, for debugging decisions. |

Conventions:
- **Units:** newtypes (`Watts`, `WattHours`, `EurPerKwh`, `Soc`). Internally everything is W, Wh and €/kWh, with explicit conversions at the edges. DAO mixes W, kW, kWh-per-interval, J/cm² and W/m², and has bugs from it.
- **Time:** `jiff`.
  - Timestamps are UTC internally. A slot is the UTC start of a 15-minute interval.
  - Europe/Amsterdam is used only at the edges: day boundaries, display, config times.
  - DST days naturally have 92 or 100 slots.
- **Errors:** `thiserror` in libraries, `anyhow` in the binary. No panics in the service loop.
- **Logging:** `tracing`. Every decision is logged on one line with its reason.

## 8. Configuration

The app options come from `/data/options.json`, and a standalone `dess.toml` uses the same schema. Here is George's DAO config, translated:

```yaml
victron:
  host: 192.168.1.20
  pv_relay: 2                    # Cerbo relay driving the PV contactor (function: Manual)
  pv_relay_energized: pv_off     # George's site: de-energised = PV on
grid:
  max_import_kw: 17
  max_export_kw: 17
battery:
  wear_cost_eur_per_kwh: 0.0     # optional
  reserve_soc: 0                 # optional: always keep this much for outages
pv:                              # optional: starting point and sanity check; learned from history anyway
  - { kwp: 5.59, tilt: 33, azimuth: 193 }   # compass degrees, 180 = south (DAO's 0 = south)
  - { kwp: 3.44, tilt: 9,  azimuth: 193 }
prices:
  area: NL
  entsoe_token: "…"              # optional; fallback source and history older than 2 months
tariff:                          # €/kWh excl. VAT; keys are the dates each value takes effect
  vat:          { 2023-01-01: 0.21 }
  energy_tax:   { 2025-01-01: 0.10154, 2026-01-01: 0.09161 }
  markup_buy:   { 2025-12-14: 0.01504 }
  markup_sell:  { 2025-12-14: 0.01504 }
  net_metering_until: 2026-12-31
weather:
  station:                       # optional, read from HA
    temperature: sensor.ws90_outdoor_temperature
    irradiance: sensor.ws90_solar_radiation
loads:
  heat_pump:
    energy: sensor.kwh_meter_warmtepomp_energy_import
    backed_up: true
    openamber: true              # optional: read DHW schedule/legionella entities for DHW timing
  # ev:                          # brother
  #   power: sensor.ev_charger_power
  #   backed_up: false
history:                         # optional, only used by `import`
  grid_import: sensor.p1_meter_energy_import
  grid_export: sensor.p1_meter_energy_export
  pv: sensor.pv_omvormer_energie
  battery_in: sensor.accu_ac_laadenergie
  battery_out: sensor.accu_ac_ontlaadenergie
  soc: sensor.batterij_soc
```

Location (lat/lon/elevation) and time zone come from HA's core config. Where each DAO setting goes:

| DAO setting | dess-oxide |
|---|---|
| `charge_stages`, `discharge_stages`, `dc_to_bat_*`, `bat_to_dc_*`, `minimum_power` | learned (§12.2) |
| `capacity` | learned from SoC vs DC energy; the BMS's installed capacity is the prior |
| `lower_limit`, `optimal_lower_level` | Victron's ESS minimum SoC (read) plus `reserve_soc` |
| `min/max_soc_einde_opt` | gone; the price tail values stored energy (§14.2) |
| `yield_factor`, `ml_prediction`, `xgboost` | learned PV model (§12.3) |
| `baseload`, `use_calc_baseload`, `baseload_calc_periode` | learned load model (§12.4) |
| `heating` block | heat pump forecaster (§12.5) |
| `scheduler` | gone; event-driven |
| `database_ha`, `database_da` | HA WebSocket; own SQLite in `/data` |
| `graphics`, `report`, `notifications`, `dashboard` | HA |
| `machines` | a "cheapest start" time on the page, optionally an HA entity for automations (§14.4) |
| `meteoserver_key`, `tibber` | not needed |

## 9. Victron interface

### 9.1 Transport: the Cerbo's local MQTT

Venus OS mirrors its entire D-Bus onto a local MQTT broker (FlashMQ):
- `N/<portal>/<service>/<instance>/<path>` publishes values,
- `W/…` writes,
- `R/<portal>/keepalive`, sent at least every 60 s, keeps the stream alive.

We use this instead of Modbus TCP, for these reasons:

- **Missing values arrive as `null`.** The Modbus server returns 0 for a missing value, so "0" and "not available" can't be told apart. That is dangerous for SoC and power readings.
- **Push instead of polling.** Values update about once a second.
- **The full namespace, with no register table to maintain.** That includes hub4 overrides and releasing them, DVCC's operational limits, full-precision floats, and the whole 48-slot Dynamic ESS (DESS) schedule should we want it.
- **Stable paths.** Topics are the D-Bus paths documented in Victron's dbus wiki, the same ones Victron's own gui-v2 uses.
- **Already enabled** at George's site ("MQTT on LAN"), where the Modbus TCP server is off.
- **No new dependency.** `rumqttc` is already in the stack for HA.

If MQTT ever proves problematic, the same data is on Modbus TCP. The register map is in Victron's `dbus_modbustcp` `attributes.csv`, and `probe` can be extended for it.

The `dess-victron` crate provides:
- typed D-Bus paths;
- a client with keepalive and reconnect;
- a `Snapshot` that tracks the age of every value;
- a fake Cerbo for tests (§18).

### 9.2 George's Cerbo

Read-only snapshot from 2026-09-27:

| | |
|---|---|
| Venus OS | v3.66. Fine for this design; no update needed. |
| ESS | Optimized without BatteryLife (state 10), Hub4Mode 1 (total of all phases), minimum SoC 5 % |
| Grid metering | **No grid meter** (`RunWithoutGridMeter = 1`). The MultiPlus AC-in is the grid measurement. That's fine because all loads are on AC-out. The HomeWizard P1 meter in HA stays useful for reconciling against the bill. |
| Battery | JK-BMS on the BMS-Can port (`battery/512`): 628 Ah (≈ 32 kWh at 51.2 V), CCL/DCL 247 A, CVL 56.8 V. **SoC is reported in whole percent** (§12.2). |
| PV | An energy meter on USB/RS485 in the "PV inverter on output" role (`pvinverter/31`). The inverter itself can't be curtailed in software, hence the contactor. |
| Relay 2 | Function Manual, polarity normal, `InitialState` 0, currently open while PV produces. So **de-energised = PV on, and it boots open**: the fail-safe wiring is already in place. |
| Dynamic ESS | Mode 0 (off). 48 stale slots from June 2025 remain, all in the past, so they can never match. Harmless. |
| Today's control | DAO's chain writes `settings/0/Settings/CGwacs/AcPowerSetPoint` many times a minute (6 changes in 25 s). |

About that last row:
- `AcPowerSetPoint` is the **persisted** ESS setting (Modbus 2700), stored on the Cerbo's flash.
- Victron added the volatile `hub4/0/Overrides/Setpoint` (Modbus 2716) in Venus 3.50 specifically for frequent updates.
- Switching the existing HA automation to write the override is worth doing now, independent of dess-oxide.

### 9.3 What we read

| Quantity | Path | Notes |
|---|---|---|
| SoC | `battery/<n>/Soc` | whole percent on the JK-BMS; refined by our estimator (§12.2) |
| Battery V / I / P | `battery/<n>/Dc/0/{Voltage,Current,Power}` | + = charging |
| BMS limits | `battery/<n>/Info/{MaxChargeCurrent,MaxDischargeCurrent,MaxChargeVoltage}`, `InstalledCapacity` | |
| Effective limits | `vebus/<n>/BatteryOperationalLimits/*` | what DVCC actually passes to the Multis |
| Consumption per phase | `system/0/Ac/ConsumptionOnOutput/L{1,2,3}/Power`, `…OnInput…` | "on input" is where the brother's EV shows up |
| Grid per phase | `system/0/Ac/Grid/L{n}/Power` | + = import; equals the MultiPlus AC-in at George's site |
| PV on AC-out | `system/0/Ac/PvOnOutput/L{n}/Power`, `pvinverter/<n>/Ac/Energy/Forward` | |
| MultiPlus AC-in, AC-out, DC | `vebus/<n>/Ac/ActiveIn/L{n}/P`, `Ac/Out/L{n}/P`, `Dc/0/Power` | for efficiency learning |
| VE.Bus state, mode, alarms | `vebus/<n>/{State,Mode,Alarms/*}` | |
| Grid presence | `system/0/Ac/ActiveIn/Source`, `vebus/<n>/Ac/ActiveIn/ActiveInput` | 240 = disconnected, i.e. an outage |
| ESS settings | `settings/0/Settings/CGwacs/{Hub4Mode,BatteryLife/State,BatteryLife/MinimumSocLimit}`, `system/0/Control/ActiveSocLimit` | verified, never changed |
| Override state | `hub4/0/Overrides/{Setpoint,MaxChargePower,MaxDischargePower,ForceCharge}` | readback of our writes |
| Relays | `system/0/Relay/{0,1}/State`, `settings/0/Settings/Relay/*/{Function,Polarity,InitialState}` | |

**Data hygiene.**
- `null` means unavailable.
- Every value carries its age. Key values older than 10 s count as stale.
- Plausibility checks run on top: SoC can't jump 50 % between samples, voltages must be in range, and so on.
- The planner never acts on stale or implausible data, and the executor releases control (§15.3). There are no "assume SoC is 50 %" defaults, which is what DAO does.

### 9.4 What we write

| Path | Purpose | Persisted on the Cerbo |
|---|---|---|
| `hub4/0/Overrides/Setpoint` | grid setpoint from the 1 Hz loop (§15) | no (RAM) |
| `system/0/Relay/1/State` | PV contactor (relay 2), written only on change | yes |
| `settings/0/Settings/CGwacs/BatteryLife/MinimumSocLimit` | outage backstop only (§16) | yes, rarely |

Every write goes to `W/…` and is verified against the next `N/…` update. Nothing else is ever written.

### 9.5 Hardware figures (priors, all refined by learning)
- **Per MultiPlus-II 48/5000:**
  - 4000 W continuous at 25 °C, falling to 3700 W at 40 °C.
  - 70 A charger.
  - 18 W at zero load (12 W in AES mode). Community measurements put it at 20–24 W per unit in ESS.
- **For three units:** about 12 kW continuous. George's JK-BMS allows 247 A (≈ 12.8 kW); DAO's config caps charging at 10 kW and discharging at 11 kW.
- **Efficiency:** Victron publishes no efficiency curves, so learning them (§12.2) is the only route.

## 10. External data

### 10.1 Prices
- **Primary: Nord Pool data portal.**
  - Request: `GET https://dataportal-api.nordpoolgroup.com/api/DayAheadPrices?date=<CET day>&market=DayAhead&deliveryArea=NL&currency=EUR`.
  - Returns 96 entries of 15 minutes, in €/MWh with UTC timestamps. Returns HTTP 204 before publication.
  - Anonymous access covers only about the last 2 months. The endpoint is unofficial (HA's own Nord Pool integration uses it), so poll sparingly.
  - Next-day results arrive around 12:55 CET/CEST.
- **Fallback: ENTSO-E.**
  - Uses document type A44 with the token from config. Returns XML with PT15M periods; values that don't change are omitted and have to be forward-filled.
  - It had several NL data gaps in 2026, so it's the fallback, not the primary.
  - It is also the source for price history older than 2 months (backtests).
- **Storage:** we store the raw spot price per slot. The tariff is applied at planning time, so a tariff change also applies retroactively in backtests.

### 10.2 Weather
- **Forecast:** Open-Meteo, `https://api.open-meteo.com/v1/forecast` with `models=knmi_seamless`.
  - That model is KNMI Harmonie-AROME NL (2 km, hourly runs, about 2.5 days ahead), continued with ECMWF IFS. It's the same KNMI model DAO gets via Meteoserver, and it needs no key.
  - Variables:
    - `shortwave_radiation` (GHI)
    - `direct_normal_irradiance`, `diffuse_radiation`: Open-Meteo's own split of KNMI's global radiation
    - `temperature_2m`, `relative_humidity_2m`, `dew_point_2m`, `wind_speed_10m`, `cloud_cover`. Humidity drives the heat pump's defrost losses (§12.5).
  - The native data is hourly and backward-averaged. We convert it to 15-minute slots ourselves, keeping the energy the same.
  - The free tier is for non-commercial use, which covers home use. We make roughly 50 calls a day, against a limit of 10,000.
  - Meteoserver can be added later as an alternative source if Open-Meteo proves worse.
- **Training data:** Open-Meteo's historical forecast API (`historical-forecast-api.open-meteo.com`) has KNMI Harmonie NL from 2024-07-01. So models train on the same forecast product they predict from, which avoids DAO's #835 class of bug.
- **Observed irradiance** is used in two ways:
  - **WS90** (via HA): live on-site GHI and temperature, for nowcasting and for learning forecast bias.
  - **Open-Meteo satellite API:** 10-minute MTG data with about 20 minutes delay; the SARAH3 archive goes back decades. It's a clean training target for the irradiance-to-PV mapping, and the only observation source for sites without a station.

### 10.3 Home Assistant inputs
- **Connection:** WebSocket `ws://supervisor/core/websocket`, authenticated with `SUPERVISOR_TOKEN` (the app needs `homeassistant_api: true`).
- **Live values:** `subscribe_entities` for the configured entities: WS90, the heat pump meter, OpenAmber's DHW schedule (`Tapwater starttijd/eindtijd`, weekday switches, next legionella run), and the brother's EV power.
- **History:** `recorder/statistics_during_period` for backfill. Hourly statistics are kept forever; 5-minute statistics only for about 10 days.
- **Location:** `GET /api/config`.
- **Later:** the HomeWizard local API directly (1 s data), if the HA route turns out to be too coarse.

## 11. Storage and history bootstrap

### 11.1 Storage

SQLite at `/data/dess.db` (`rusqlite` with `bundled`, WAL mode). Tables:

| Table | Content |
|---|---|
| `slot_measurements` | Per 15-minute slot: grid import/export, PV, AC consumption, HP, EV, battery AC in/out, battery DC, SoC start/end, `pv_curtailed`, `grid_lost` |
| `prices` | slot, area, €/MWh, source, fetched_at |
| `weather_forecasts` | issued_at, slot, GHI/DNI/DHI, temperature, wind, cloud, source. Vintages are kept because training needs "what we knew then" |
| `observations` | WS90 and satellite values per slot |
| `efficiency_bins` | Steady-state conversion samples aggregated per direction and power bin, with slow forgetting |
| `forecasts` | issued_at, slot, kind, p10/p50/p90, for accuracy tracking |
| `plans`, `actions` | Every plan, plus an audit log of every write to the Cerbo and its readback |
| `models` | name, version, trained_at, metrics, weights (burn record) |

That's about 35k slots a year, so everything stays small. Measurements are kept forever; forecasts, plans and actions for 90 days.

### 11.2 Bootstrap (`import`)

On first start the app backfills history so that models are useful on day one:

1. **HA hourly long-term statistics** for the `history:` entities, going back to when HA started recording them. House load is derived with the load identity (same as DAO). The last 10 days are also fetched at 5-minute resolution.
2. **Past KNMI forecasts** from Open-Meteo for the same period (back to 2024-07-01).
3. **Prices:** Nord Pool for the last 2 months, ENTSO-E before that.

Hourly history is coarser than we want, but it covers seasons. That matters most for the heat pump's temperature response and PV geometry. From then on, our own 15-minute data takes over.

The efficiency curve needs high-resolution data, so it starts from a prior, George's DAO stage table, and converges after a few days of running.

## 12. Models

### 12.1 Approach
- **Framework:** grey-box models in [burn](https://burn.dev) 0.21, using the ndarray backend with autodiff. It's pure Rust on the CPU and builds for aarch64 and amd64. Training runs in-process nightly and takes seconds: the data is tens of thousands of rows.
- **Physics structure with learnable parameters, not generic networks.** The data is small. Models must extrapolate, e.g. to a colder winter than any seen. The parameters should be meaningful, so they're published as HA sensors that you can sanity-check.
- **Features:** computed once in plain Rust (sun position, clear-sky irradiance, calendars) and fed in as tensors.
- **Baselines and the promotion gate:**
  - Every model has a simple baseline next to it.
  - A model is used only if its rolling backtest error beats the baseline.
  - At runtime, the app falls back to the baseline when a model is stale or its live error degrades.
- **Outputs:** every forecast is energy per 15-minute slot (Wh), with P10/P50/P90 where it matters.
- **Versioning:** burn is pinned. The models are small, so framework upgrades are cheap.

### 12.2 Battery and inverter (system identification)

Learned from the ~1 s Victron samples:

- **What we measure:**
  - AC side: the MultiPlus conversion power, AC-in minus AC-out (`vebus/…/Ac/ActiveIn/L{n}/P` minus `Ac/Out/L{n}/P`), summed over phases. AC-coupled PV passes through, so it cancels out.
  - DC side: battery power minus DC PV (zero at George's site).
- **Steady-state filter:** keep samples where power is stable within ±3 % for at least 20 s. Leave out transitions and charge-stage changes. Samples are aggregated into `efficiency_bins`.
- **Loss model per direction:** `loss(P) = a + b·|P| + c·P²`, where `a` is idle loss, `b` covers switching and linear losses, and `c` is resistive. This gives the planner a smooth, physically shaped efficiency curve.
- **SoC estimator:** George's JK-BMS reports whole percent (320 Wh steps) and its coulomb counter drifts between full charges. We keep our own estimate: integrate battery DC power, re-anchor at each BMS step and at 100 % (a small Kalman filter). The planner's 100 Wh grid and the 1 Hz loop need it.
- **Usable capacity:** over long one-way stretches (ΔSoC ≥ 30 %), `C = ∫P_dc dt / ΔSoC`. The difference between charge and discharge stretches gives the battery's own DC round-trip efficiency. Tracking capacity fade comes for free.
- **Power limits:** the live DVCC/BMS charge and discharge current limits are read live from the Cerbo. For future slots, the planner uses a learned curve of maximum charge power vs SoC (the taper near full).
- **Prior until enough data:** a loss curve fitted to George's DAO stage table (71 % at 300 W charge, which is mostly idle loss from three units) and 32 kWh.

### 12.3 PV

The goal is to learn everything from history. Watt-peak and orientation in config are optional: they seed the model and serve as a sanity check.

- **Inputs per slot:**
  - sun position, averaged over three sub-steps
  - forecast GHI, DNI and DHI
  - air temperature and wind
- **Model (burn):** K virtual arrays. Each has a learnable kWp (softplus), tilt in [0°, 90°] (sigmoid) and azimuth. On top of those:
  - Hay–Davies transposition to plane-of-array irradiance
  - a Faiman cell-temperature model with a learnable temperature coefficient
  - a shading map: a learnable grid over sun (azimuth, elevation) in 5° cells, mapped through a sigmoid to a [0, 1] factor on beam irradiance and smoothed with a total-variation penalty
  - a learnable soft clip for the inverter's AC limit
- **Initialisation:**
  - from the config arrays when present;
  - otherwise from a non-negative least-squares fit over a fixed set of candidate orientations (E/SE/S/SW/W at 10°, 30° and 45° tilt, plus flat). The dominant weights pick K and the starting values.

  The virtual arrays don't have to match the physical strings: we only see the inverter total, so all that matters is predicting it.
- **Two-stage training:**
  1. Fit the physical mapping on *observed* irradiance: satellite, or WS90 once it has history. This gives a clean irradiance-to-PV model.
  2. Fit a calibration from *forecast* to observed irradiance, by clear-sky index and lead time. It also produces the quantiles.

  This keeps weather-forecast error apart from system behaviour.
- **Training filter:**
  - Slots where PV was curtailed, clipped by an export limit, or islanded are excluded, because they would corrupt the fit.
  - Snow and outliers are handled by a Huber loss.
- **Nowcast:** for the next ~3 h, blend in the recent ratio of actual to predicted PV and the WS90/satellite clear-sky index, decaying with lead time.
- **Published:** effective kWp and orientation per virtual array, the temperature coefficient, and accuracy.
- **Baseline:** clear-sky PV × forecast clear-sky index.

### 12.4 House base load
- **Target:** Victron AC consumption (AC-out plus AC-in loads) minus heat pump, minus EV.
- **Baseline (seasonal-naive):**
  - a weighted mean of the same quarter hour on the last 4 same weekdays;
  - Dutch public holidays treated as Sundays;
  - scaled by the level of the last 24 h.
- **Model (burn):**
  - a small MLP (2×32) with pinball loss, outputting P10/P50/P90;
  - features: Fourier terms for time of day, weekday, holiday flags, outdoor temperature, daylight (sun elevation × cloudiness, for lighting), and the mean load of the last 24 h.
- **Evaluation:** both slot MAE and 4-hour block error. Load is spiky (kettle, oven, dishwasher), while the planner mostly cares about energy per slot and over expensive blocks.

### 12.5 Heat pump
- **Target:** per-slot energy from the HomeWizard meter.
- **Split between DHW and space heating:** use OpenAmber's DHW-demand binary sensor if it's available. Otherwise use the DHW schedule window plus the power signature.
- **Why degree-days aren't enough:** humidity-driven defrosting.
  - Frost builds on the outdoor coil when the coil runs below 0 °C in humid air. That is worst from about −3 to +5 °C in fog or drizzle.
  - Cold, dry air, like a clear −8 °C day, holds far less water and frosts much less, and sunny days add solar gains.
  - So a foggy 0 °C day can use *more* energy than a sunny −8 °C day. A degree-day model gets this backwards.
- **Space heating (burn grey-box):**
  - `heat = softplus(UA·(T_bal − T_eff) + w·wind − g·I_solar)`
  - `T_eff` is a learned mixture of outdoor-temperature moving averages with time constants {3, 6, 12, 24, 48} h. The mixture weights capture the building's thermal lag.
  - `COP(T_out) = c0 + c1·T_out`, bounded.
  - **Defrost penalty:** `frost = max(0, ω_air − ω_sat,ice(T_out − ΔT_coil))`, the water in the air above what the colder coil surface can hold. `ΔT_coil` is learnable, around 5–8 K.
  - Electrical power = `heat / COP(T_out) × (1 + k·frost) + standby`.
  - OpenAmber publishes a defrost binary sensor. We learn the defrost time fraction against temperature and humidity as a second training target, so `k` and `ΔT_coil` are well pinned down rather than guessed from total energy.
  - A learnable time-of-day profile captures any night setback.
  - The parameters (balance point, UA, wind and solar terms, COP slope, defrost sensitivity) are all meaningful and published.
- **Weather features:**
  - forecast: temperature, relative humidity, dew point, wind, irradiance, cloud;
  - observations: WS90 temperature and humidity.
- **DHW:**
  - Runs happen inside OpenAmber's tap-water schedule (start/end time and weekday switches) and at legionella runs (next-run datetime entity).
  - Energy per run is learned against outdoor temperature (the COP of an air-source heat pump) and season (cold-water temperature).
  - The expected energy is spread over the slots where a run is likely.
- **Baseline:** degree-day regression per hour of day.
- **Outage reserve:** the `backed_up` flag decides whether it counts toward the reserve (§16).

### 12.6 EV (brother's site)
- **Wiring:** the charger is on the Victron AC-in side. The grid meter and Victron's consumption on input include it, but it is not backed up.
- **v1:**
  - EV energy is measured, from an entity or from Victron's consumption on input, and left out of base-load training. It is not forecast.
  - When charging starts, the next replan sees it.
  - The executor's loop stops the house battery from draining into the car in cheap slots (§15.2): the battery helps only when stored energy is worth less than the current buy price.
- **Later:** forecasting EV sessions, and smart charging through the charger's API.

### 12.7 Accuracy and savings
- Every forecast is stored with its issue time. A nightly job computes MAE and bias per lead-time bucket (0–6 h, 6–24 h, 24–36 h) for PV, load and heat pump. Results go to sensors and the plan page.
- Planner quality is realised cost from measurements, compared against three baselines:
  1. no battery,
  2. plain ESS self-consumption,
  3. the perfect-foresight optimum (hindsight).

  The savings sensors come from this comparison, not from the plan's own predictions.

## 13. Tariffs

All components are date-effective, as in DAO.

```
buy(t)  = (spot + markup_buy + energy_tax) × (1 + vat)
sell(t) = (spot + markup_sell + energy_tax) × (1 + vat)   while net metering applies (DAO's tax_refund formula)
        = (spot + markup_sell) × (1 + sell_vat)           after net_metering_until (markup_sell is then usually ≤ 0)
```

- **End of net metering:** salderen ends on 2027-01-01 for everyone. After that:
  - every imported kWh pays full energy tax and VAT;
  - export earns roughly spot (minus fees), per the supplier's terms;
  - the buy/sell spread widens by roughly €0.13–0.15/kWh.

  Battery-to-grid arbitrage becomes rarer, while self-consumption and PV curtailment at negative prices matter more. The planner needs no change for this, only the tariff.
- **Annual netting cap:** ignored. George is presumably a net importer, and only three months of net metering are left.
- **Energy tax 2026:** €0.09161/kWh excluding VAT (George's DAO config has 0.09157). The 2027 proposal is about €0.0880, not yet law.
- **Fixed costs** (standing charge, capacity-based grid fee, tax reduction) don't affect decisions and are ignored.
- **Negative prices:** buy can go negative when spot is well below minus the tax. The planner then imports as much as allowed and curtails PV.

## 14. Planner

### 14.1 Horizon
- Slots run from now to the last known price: the end of D, or the end of D+1 after about 12:55. That is 11–35 h.
- The horizon is always extended to at least 48 h with an **estimated price tail**. The estimate is the median of the same quarter hour over the last 14 days, blended toward the daily mean and marked as estimated. It gives stored energy a sensible value at the end, without DAO's end-SoC helpers.
- Only the first slot is executed.
- **Backlog: a better price tail.** Only worth building if it pays:
  1. Measure the value of perfect information first: replay history with the estimated tail and again with the real prices as if they'd been known. The difference is the most a price forecast could ever earn.
  2. If that's material, forecast the day-ahead price from residual load (demand − wind − solar), using ENTSO-E's day-ahead wind, solar and load forecasts for NL and DE (George has a token) or NED's production forecasts. A small regression per hour of day would do. Paid services such as wattwanneer.nl aren't needed.

### 14.2 Dynamic programming
- **State:** battery energy E (DC side) on a 100 Wh grid (321 states for 32 kWh) × PV relay state (on/off).
- **Decision per slot:** the next E', reachable within the learned SoC-dependent power limits, and the relay state u.
- **Transition** for each (E, E', u):
  1. `P_dc = (E' − E)/Δt`
  2. `P_ac = P_dc ± loss(P_dc)`, using the learned curve
  3. `grid = load + hp + ev − pv·u + P_ac + idle`
  4. Reject the transition if it breaks the grid import/export limits or the inverter limits.
- **Stage cost:** `buy·grid⁺ − sell·grid⁻ + wear·|P_dc|·Δt`, plus a small penalty for relay toggles.
- **Bounds:** per-slot lower bound `E_min(t) = max(ESS minimum SoC, reserve_soc, outage reserve trajectory)`.
- **Solve:** a backward pass computes the value function V_t(E), and a forward pass extracts the plan. About 200 slots × 642 states × ~60 transitions ≈ 8M evaluations, which takes milliseconds.
- **Outputs per slot:** battery AC power, SoC trajectory, grid power, relay state and cost. Also the value function V_t(E) and its slope λ_t = ∂V/∂E, the marginal value of stored energy. The executor uses V every second to decide whether the battery or the grid covers forecast errors (§15.2). λ also drives the explanations and the cheapest-start sensor.

Why DP and not a MILP:
- It is exact for a single battery.
- It needs no native solver: HiGHS and CBC mean C++ builds for every architecture.
- It is deterministic.
- It handles any efficiency curve and SoC-dependent limits without SOS2 tricks.

If multiple storage units ever arrive, for example with EV smart charging, the planner sits behind a trait. `good_lp` with pure-Rust `microlp` would be the next step.

### 14.3 Uncertainty
- v1 plans on P50 forecasts, and replanning every 15 minutes absorbs the errors. That's safe because the executor applies the plan's value function to measured load and PV every second (§15.2) rather than replaying planned numbers.
- The outage reserve uses P90 load and P10 PV (§16).
- Later: penalise plans that depend on PV turning up exactly on time, such as scenario-based planning.

### 14.4 Cheapest start (replaces DAO's `machines`)

"Cheapest start" gives the start time of a flexible run (default 3 h at about 1 kWh, within a 20:00–08:00 window; both configurable).

- It's computed by re-running the DP with that extra load added at each candidate start, which takes about 40 runs × a few ms.
- So it reflects the *true* marginal cost, battery and PV included, not just the spot price.
- An HA automation starts the dishwasher at that time.

## 15. Executor

### 15.1 Who absorbs forecast errors?

The plan will be off in every slot. When the house uses more than forecast, or PV comes in lower, the missing energy has to come from either the grid or the battery. Both simple rules get this wrong half the time.

**"Battery follows the plan, grid absorbs"** is what target-SoC control does, including Victron's Dynamic ESS (DESS).
- Take a slot where we export from the battery at a high price. A surprise 2 kW load cuts the export by 2 kW, and every one of those kWh loses the sell price.
- Yet the battery holds energy worth less than that sell price. That is exactly why the plan was exporting.
- Victron's source (`dynamicess.py`) confirms this behaviour. In a discharge slot it caps inverter output at the computed rate, so extra load eats into the export. Its "pro-grid" strategy is no better: once load exceeds the rate, it drops to plain self-consumption, which stops the export entirely.

**"Grid follows the plan, battery absorbs"** is what a fixed grid setpoint does.
- Take a cheap slot where the plan buys from the grid and saves the battery for tonight's peak.
- If the brother's EV starts charging, the battery drains into the car.

The right choice depends on what a kWh in the battery is worth right now, compared with the grid price. The planner already knows that. The DP's value function `V_{t+1}(E)` is the expected cost from the next slot onward if we arrive there with stored energy E. Its slope λ is the marginal value of stored energy.

### 15.2 The control loop

Every second, the executor solves a one-slot problem using *measured* load and PV instead of forecasts:

```
for each candidate battery power b (100 W steps, within live BMS/DVCC/inverter limits):
    grid(b)  = consumption − pv + P_ac(b)
    E_end(b) = E_now + P_dc(b) · τ                         # τ = time left in the slot
    cost(b)  = price(grid(b)) · grid(b) · τ + wear · |P_dc(b)| · τ + V_{t+1}(E_end(b))
b* = argmin cost;   setpoint = grid(b*)
```

Here `price` is the buy price for import and the sell price for export.

This is the planner's own policy, applied to what is actually happening:

| Slot | Surprise load | Surprise PV |
|---|---|---|
| Exporting from the battery at a high price | battery covers it, **export stays on plan** | stored if λ·η > sell, otherwise exported |
| Charging from the grid at a low price | grid covers it, charging continues | stored, instead of grid charging |
| Self-consumption at a high price | battery covers it, grid stays ≈ 0 | stored |
| Holding the battery for a later peak | **grid covers it** (the EV case), battery untouched | stored if λ·η > sell |
| Sell price < 0 | – | the planner already opened the PV relay for this slot |

At the edges it stops being a fixed rule, smoothly. Suppose a surprise drains the battery far enough that the value of what's left rises above the grid price. Then the grid takes over.

As a result, SoC drifts from the plan. The next replan starts from the measured SoC. It runs every slot, and immediately when SoC is more than 3 % off.

**Implementation notes:**
- **Victron's ESS still regulates at sub-second speed:** phase compensation (Hub4Mode 1), BMS CCL/DCL, the ESS minimum SoC, ramp limits. We only move the setpoint.
- **Setpoint writes:**
  - They go to the volatile `hub4/0/Overrides/Setpoint`, not the persisted setting (§9.2).
  - A deadband avoids chasing noise: write when the value changes by more than 50 W, or every 10 s.
- **PV relay:** decided per slot by the planner. It switches only at slot boundaries, plus the safety overrides in §16.
- **No solver in the loop:** `V_{t+1}` is kept from the last plan, so each second is about 100 candidate evaluations.

### 15.3 Failure behaviour

A volatile override stays in force until it is released or the Cerbo reboots. Victron has no timeout for it, so safety comes in layers:

| Situation | Behaviour |
|---|---|
| Fresh install | shadow mode: plan and publish, no writes. Writing needs **two locks**: `control: true` in the app options (which no HA automation can flip) and the control toggle on the dess-oxide page |
| Another controller writes the ESS setpoint | refuse to take control, and drop back to shadow if already active. This catches a DAO chain (or VRM DESS) that is still running. `probe` already detects these writes |
| Control switched off, or SIGTERM (app update, HA restart, backup) | release the override, which means plain ESS; relay to PV on; restore minimum SoC |
| Task panic | the supervisor restarts the task. A panic hook releases the override if the whole process goes down |
| Inputs stale for > 10 s, or no price for the current slot | release the override: plain ESS |
| Grid lost | island: release the override, PV on (§16) |
| HA host or network down | the last setpoint persists until we're back. ESS minimum SoC and the BMS bound it: the worst case is one unplanned charge or discharge. HA gets our MQTT last-will (`offline`), which can trigger a notification automation |
| Cerbo reboot | the override is gone, so plain ESS; the relay boots open (PV on) per `InitialState` |

This is already strictly better than today's DAO chain, which has no release path at all.

If the "HA host or network down" row ever matters, there are two options:
- **A Cerbo-side watchdog:** a small script under `/data`, which survives firmware updates. It releases the override when it hasn't been refreshed for 60 s. This is the only hard guarantee.
- **The DESS executor** (§15.4).

### 15.4 Alternative executor: Victron's DESS local schedule

Kept behind the same trait.

- **How it works:** in mode 4 ("Local"), Venus's built-in DESS controller executes a schedule we write: a target SoC and strategy per slot, 48 slots via MQTT.
- **Advantage:** fail-safety. The schedule keeps running without us for up to 12 h, then falls back to plain ESS.
- **Drawback:** the problem in §15.1. It steers SoC along the plan and lets the grid absorb every deviation, so it can't keep an export on plan.
- **When to use it:** if hard fail-safety ever matters more than the last few percent of savings.
- **Upcoming change:** Victron is moving DESS into its own service (`com.victronenergy.dynamicess`). The settings paths stay the same.

## 16. Outage preparedness

**Controls** (on the dess-oxide page; optionally mirrored as HA entities, §17):
- outage expected: on/off
- outage start (defaults to tomorrow 08:00 when switched on without a time)
- outage duration (hours, default 4)

**Behaviour:**
- **Planning (built):**
  - The window's slots are islanded in the DP: the PV relay is forced on, any grid import costs the shortfall penalty, and export earns nothing. The reserve then falls out of the plan: it's the energy needed to get through the window without import.
  - Margins until quantile forecasts exist: load ×1.3 and PV ×0.7 inside the window. The target is `R = Σ_window (P90 base load + P90 backed-up heat pump − P10 PV) / η_discharge + ESS minimum SoC`; the EV is not backed up and is left out.
  - Charging for the outage therefore happens in the cheapest slots before the window. It doesn't simply charge to 100 % right away.
- **Backstop (built):** from three hours before the window until its end, Victron's ESS minimum SoC (`BatteryLife/MinimumSocLimit`) is raised to the plan's SoC at the window start (minus 3 points, never above the current SoC). Plain ESS, our setpoint override and the DESS controller all honour it, so the Victron keeps the reserve even if dess-oxide or HA dies. The original minimum is remembered in SQLite and restored after the window. It needs both locks, like the setpoint.
- **PV:** the relay is forced on during the window. The Victron can then throttle the AC-coupled PV while islanded (frequency shifting), and PV can recharge the battery.
- **Grid-loss detection:**
  - When the active input reads 240 (disconnected) or VE.Bus raises its grid-lost alarm, the page shows it (and `binary_sensor.dess_grid`, if enabled, turns off) and control drops to "island": PV on, no other actions.
  - With the optional entities enabled, an HA automation can send a notification. This also covers outages nobody announced.
- **Always-on reserve:** a reserve SoC on the page (default from config).
- **Later:** automatic triggers, such as KNMI code orange/red warnings or grid-operator planned-outage notices.

## 17. Home Assistant integration

George doesn't want HA cluttered with entities and their recorder history. So dess-oxide's UI is its own page, and HA entities are opt-in.

### 17.1 The dess-oxide page (default)

It's served through HA ingress, so it opens from the HA sidebar with HA's own login. There's no exposed port. It has:
- **Plan:**
  - a chart of prices, SoC, battery power, PV, load and heat pump over the horizon;
  - the current slot's decision, and why it was made.
- **Forecasts:** PV, load and heat pump for today and tomorrow, and their accuracy per lead time (§12.7).
- **Money:** today's and this month's cost, and savings against the baselines. During the rollout (§18), the side-by-side comparison with DAO goes here too.
- **Learned values:** the efficiency curve, usable capacity, standby loss, PV orientation, and the heat pump parameters.
- **Status:** shadow / active / fallback / island / error; the last Victron update, price fetch and plan; the probe findings.
- **Controls:**
  - the outage window (§16)
  - reserve SoC
  - a manual override (auto / self-consumption / hold / charge / discharge), which reverts at midnight
  - replan now
  - control on/off. This works only when the app option `control: true` is also set: the two locks of §15.3.

It's server-rendered by axum, with a vendored chart library and small forms for the controls. There's no JS build step. Control state is stored in SQLite, so it survives restarts.

### 17.2 Optional HA entities

These are off by default, enabled with an app option. They're for automations that need dess-oxide's state, such as starting the dishwasher at `cheapest_start` or a notification when the grid is lost.
- **A small fixed set:** status, current mode, SoC target, `cheapest_start`, grid present, the outage switch and start time, and today's savings.
- **Low update rate:** at most every 15 minutes, or when a value changes. Nothing updates every second.
- **No large attributes:** the plan itself stays on the page.
- **Delivery:** MQTT device-based discovery (`homeassistant/device/dess_oxide/config`) on the broker HA's MQTT integration already uses. At George's site that's the Cerbo; no Mosquitto app is needed.
  - It's a separate connection from the Victron client, and it can only publish under `homeassistant/…` and `dess_oxide/…`.
  - Discovery is republished on every connect and whenever HA announces itself on `homeassistant/status`, so it doesn't depend on the broker keeping retained messages.

## 18. Testing, simulation and backtesting

- **`dess-core`:**
  - unit tests;
  - `proptest` properties: the planner never breaks bounds, and DP cost ≤ self-consumption cost on the same forecasts;
  - date-effective tariff switches;
  - DST days give 92 or 100 slots.
- **`dess-models`:**
  - sun position against NREL SPA reference values;
  - transposition against pvlib reference numbers;
  - every model recovers known parameters from synthetic data (e.g. PV generated with a known tilt and azimuth is learned back within tolerance).
- **`dess-victron`:**
  - path and payload decoding tests, including `null` handling;
  - a **fake Cerbo**: an in-process MQTT broker plus a simulated battery, MultiPlus/ESS loop and relay, for executor integration tests. These include the failure scenarios from §15.3.
- **Backtest** (`dess-oxide backtest`):
  - Replays history slot by slot, with forecasts as they were available at the time (historical forecast API).
  - Reports realised cost against the three baselines in §12.7, plus forecast errors.
  - A fixed fixture runs in CI as a regression gate: one anonymised month from each site, with the expected numbers checked in.
- **Rollout alongside DAO** (at least a week at George's site before any writes):
  1. dess-oxide runs in shadow mode next to DAO. DAO keeps control.
  2. Every slot, compare:
     - **Forecasts:** PV, base load and heat pump against actuals, per lead-time bucket (§12.7).
     - **Decisions:** dess-oxide's planned setpoint against the setpoint DAO actually ran. The recorder logs the setpoint in effect for every slot from M0 on.
     - **Cost:** the actual cost under DAO against a simulation of dess-oxide's plan, replayed against the same measured loads, PV and prices with the learned battery model.
  3. A daily comparison goes to the dess-oxide page.
  4. Handover, once George is satisfied:
     - disable DAO's Victron automations,
     - set `control: true` in the app options,
     - turn control on from the dess-oxide page.
- **CI:** `cargo fmt --check`, `clippy -D warnings`, tests, the backtest gate, and the multi-arch image build.

## 19. Packaging and deployment

- **HA app:** `dess_oxide/config.yaml` with:
  - `arch: [aarch64, amd64]`
  - `image: ghcr.io/<owner>/dess-oxide`, a multi-arch manifest
  - `ingress: true` on port 8099
  - `homeassistant_api: true`
  - `init: false`
  - `options`/`schema` for §8
- **Image:**
  - A static musl binary, built in CI (`cargo-zigbuild` or `cross`).
  - The Dockerfile names its base image explicitly, since Supervisor no longer injects `BUILD_FROM`. `build.yaml` is deprecated.
  - `reqwest` uses rustls with `ring`: the default `aws-lc-rs` needs cmake and clang when cross-compiling.
- **CI:** the `home-assistant/builder` composite actions (`prepare-multi-arch-matrix`, `build-image`, `publish-multi-arch-manifest`), following `home-assistant/apps-example`.
- **Distribution:** `repository.yaml` at the repo root, so both sites add the repo URL in HA and install the same app.
- **Web:**
  - axum on the ingress port, accepting only 172.30.32.2 and honouring `X-Ingress-Path`.
  - The dess-oxide page (§17.1). It's server-rendered, with a vendored chart library (uPlot) and no JS build step.
- **Standalone:** `dess-oxide run --config dess.toml` with an HA URL, a long-lived token and MQTT settings. Used for development from a laptop against the real Cerbo, in shadow mode.

## 20. Milestones

**M0: Foundations and data collection.** Start as early as possible: data is the long pole.
- [x] workspace, config, logging
- [x] `dess-victron` reads and `probe` (tested read-only against both Cerbos)
- [x] recorder to SQLite, including efficiency bins
- [x] app packaging and CI
- [x] released as v0.1.0; installed at George's site (brother's pending)

**M1: Prices, tariff, planner in shadow mode.**
- [x] Nord Pool prices; ENTSO-E fallback still to do
- [x] date-effective tariff, including the net-exporter case during net metering
- [x] DP planner (a 48-hour plan takes about 40 ms)
- [x] baseline forecasts: load from recorded history, PV from Open-Meteo GTI
- [x] shadow planning every slot, with every plan stored
- [x] the dess-oxide page: plan, last 24 hours against DAO's setpoint, forecast errors
- [x] backfill from HA statistics (automatic, in the service)
- [ ] backtest harness

**M2: Learned models.**
- [x] battery and inverter losses from steady-state samples (standby + quadratic curve per direction)
- [x] PV (burn): effective kWp, tilt and azimuth per array, inverter cap
- [x] heat pump (burn): balance temperature, thermal lag, wind, solar, COP(T), humidity-driven frost, hot water profile
- [x] base load (burn MLP) with Dutch holidays
- [x] promotion gate against baselines on held-out days; learned values on the page
- [ ] usable capacity and SoC estimator (the JK-BMS reports whole percent)
- [ ] OpenAmber's DHW schedule as a feature; forecast accuracy per lead time

**M3: Control.**
- [x] executor with fail-safe, relay curtailment
- [ ] manual overrides (hold / charge / discharge)
- [x] outage mode with the Victron minimum-SoC backstop
- [x] the "another controller is active" interlock and the two-lock enable
- [ ] shadow-mode comparison against DAO (forecasts, decisions, cost), then handover at George's site (§18)

**M4: Brother's site and polish.**
- EV awareness
- cheapest-start sensor
- better price tail
- quantile-aware reserve
- docs

## 21. Open questions

- Brother's system: battery size, PV, relay wiring, how the EV charger is metered, and whether it has a local API.
- Export terms from 2027 with your supplier: feed-in fee and VAT on export. These set `markup_sell` and `sell_vat`.
- Brother's Cerbo: take the same read-only MQTT snapshot (`probe`) to confirm its Venus version, relay setup, grid meter and leftover DESS slots.
- Brother's contract: he moves to a dynamic contract in the coming weeks; until then his site runs shadow mode only.
- Does "HA host or network down" (§15.3) matter enough to want the Cerbo-side watchdog script? Decide after running M3 for a while.

Answered (2026-09-27):
- Venus OS 3.66 at George's site. Fine for this design; no update needed.
- Relay 2 drives the contactor. De-energised = PV on, and the relay boots open.
- Both sites used VRM Dynamic ESS in the past. At George's site the leftover slots are all from June 2025, so they're harmless, and DESS is off.
- MQTT or Modbus, whichever works best: MQTT (§9.1).
- 2026 energy tax: your DAO config has 0.09157, the official figure is 0.09161.
