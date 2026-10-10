# dess-oxide: design

dess-oxide runs a Victron ESS against Dutch 15-minute day-ahead prices. Every quarter hour it:

1. forecasts PV, house load, heat pump demand and the prices not yet published,
2. computes the cheapest battery schedule over the next 48 hours or more,
3. executes it on the Cerbo GX over the Cerbo's local MQTT, with a one-second control loop.

It ships as a Home Assistant app. Its UI is its own page in the HA sidebar (ingress). It creates no HA entities unless asked to (§12.2), and HA is not in the control path.

It replaces the Day Ahead Optimizer ([DAO](https://github.com/corneel27/day-ahead)) and the chain of HA helpers and automations that executes DAO's output. How to install and configure it is in [`dess_oxide/DOCS.md`](../dess_oxide/DOCS.md).

## 1. Goals and principles

- **Minimise the electricity bill:** import cost minus export revenue, plus battery wear.
- **Learn, don't configure.** Whatever can be measured is learned from history: conversion losses, capacity, the cells' round trip, PV orientation and yield, base load, heat pump demand, hot water. Config holds only what can't be measured: tariffs, the relay, HA entity ids, grid limits.
- **One hop to the hardware.** dess-oxide talks to the Cerbo over its local MQTT and reads back what it writes.
- **Victron stays in charge of safety.** dess-oxide moves only the grid setpoint (a volatile override), the ESS mode (for bypass), the PV relay and, around an expected outage, the minimum SoC. ESS keeps enforcing BMS limits, the minimum SoC and phase balance. Every failure it can detect releases control to plain ESS (§10.3).
- **Rolling horizon.** Replan every quarter hour and on events (new prices, forecasts, models, outage settings), each time from the measured SoC. Only the current slot is executed.
- **Pure core, I/O at the edges.** Forecasters, tariff, planner, policy and replay are deterministic functions of their inputs, with no async code and no clock, so they're unit-testable and replayable.
- **Measure ourselves.** A model is used only while it beats its baseline on held-out days. Forecast accuracy and money are on the page.

## 2. The two sites

One code path serves both; everything site-specific is config.

| | George | Brother |
|---|---|---|
| Inverter/chargers | 3× MultiPlus-II 48/5000, three-phase ESS, Cerbo GX | same |
| Battery | ≈ 32 kWh LFP, JK-BMS on CAN (reports whole-percent SoC) | LFP |
| PV | AC-coupled on AC-out, one inverter, a contactor on Cerbo relay 2 (de-energised = PV on) | AC-coupled |
| Grid | 3×25 A. No Victron grid meter: the MultiPlus AC-in is the measurement | a grid meter connected to the Victrons |
| Heat pump | Itho Daalderop Amber on [OpenAmber](https://github.com/Jordi1990/openamber), with its own kWh meter | none |
| EV | none | charger between the grid meter and the Victrons (loads on input) |

## 3. What's different from DAO

| Area | DAO | dess-oxide |
|---|---|---|
| Control path | Writes HA helpers once per run; automations carry them to the Cerbo's stored setpoint. Open loop; a failed run leaves the last values in place. | Straight to the Cerbo's volatile setpoint override, once a second, verified by readback. Released to plain ESS on any detected failure. |
| Battery losses | Hand-entered charge/discharge stages and DC constants. | Learned: inverter losses per direction, standby and bypass draw, the cells' own round trip from the BMS's counters. |
| Forecasts | PV from a hand-tuned yield; load as 24 hourly values or a weekday mean. | Learned PV geometry and inverter limit; base load (holidays, temperature, daylight); a grey-box heat pump model with frost losses; hot water from OpenAmber's mode. |
| Prices after tomorrow | Unknown; end-of-horizon SoC helpers. | Forecast by a model of wind, sun, temperature and calendar; stored energy is valued from that. No end-SoC knobs. |
| Optimiser | MILP (python-mip/CBC) with a gap and a node cap; nondeterministic when multithreaded. | Exact dynamic programming. Milliseconds, deterministic, any efficiency curve, no native solver (§9.2). |
| Forecast errors | Whatever the setpoint does. | The plan's value function decides every second whether the battery or the grid covers a surprise (§10.1). |
| Outage | Nothing. | A planned window: islanded in the plan, a Victron-side minimum-SoC backstop, PV forced on. |
| HA data | Reads HA's recorder database file. | HA's WebSocket and REST APIs only. |
| Config | About 220 fields. | About 30 lines. |

What DAO gets right and dess-oxide keeps: date-effective tariffs, a receding horizon, power-dependent efficiency, bypass when the battery has nothing to do, and the load identity `load = import − export + pv − AC into the inverters + AC out`.

## 4. Architecture

One process (tokio). Tasks share latest values through `watch` channels:

- **record** (1 s): reads the Cerbo, keeps a finer SoC (§7.2), stores 15-minute energy totals, steady-state efficiency samples and the bypass draw.
- **fetch_prices**: Nord Pool's 15-minute day-ahead prices; tomorrow's from about 12:55 until complete.
- **fetch_weather**: Open-Meteo hourly; also HA's location and language.
- **import_history**: HA's hourly statistics for the `history` sensors, every six hours (three years at first).
- **openamber**, **station**: OpenAmber's mode history and the weather station, when configured.
- **market**: prices, weather and NED for the price model (§7.7); trains it once after a start and nightly.
- **train**: PV, heat pump, hot water and base load models, after a start and nightly.
- **plan_loop**: replans at every slot boundary and on events.
- **control**: the one-second executor (§10).
- **web**: the page, through HA ingress only.
- **entities**: the optional HA entities.

Crates:

| Crate | What |
|---|---|
| `dess-core` | Pure domain logic: slots and units, recording, efficiency, capacity and cell round trip, SoC estimation, tariff, price horizon, weather correction, the DP planner, the per-second policy, the replay |
| `dess-models` | Learned models: PV, heat pump and base load (burn), hot water (statistics), prices (gradient-boosted trees) |
| `dess-victron` | The GX device's MQTT: typed readings, `probe`, and a separate write capability |
| `dess-oxide` | The binary: config, SQLite, the Nord Pool, Open-Meteo, EnergyZero, NED and HA clients, the tasks, control, the page |

CLI: `run` (the service), `probe` (read-only report on a GX device), `plan` (print the plan right now), `price-backtest` (train the price model and report its held-out error).

## 5. Victron interface

Venus OS mirrors its D-Bus onto a local MQTT broker: `N/<portal>/…` publishes values, `W/…` writes, and `R/<portal>/keepalive` keeps the stream alive. dess-oxide uses it rather than Modbus TCP: missing values arrive as `null` instead of 0, values are pushed once a second, and the whole namespace is there without a register table.

**Reads:** SoC, battery power and voltage, BMS limits and installed capacity, grid, PV and consumption per phase, the MultiPlus AC in and out, grid presence, ESS settings (mode, minimum SoC, Dynamic ESS), the relays. Values carry their age; stale or implausible data stops control.

**Writes**, only with both locks on (§10.3):

| Path | Purpose |
|---|---|
| `hub4/0/Overrides/Setpoint` | the grid setpoint, once a second (volatile) |
| `settings/0/Settings/CGwacs/Hub4Mode` | 3 (external control) for a bypass slot, back to 1 otherwise and on release |
| `system/0/Relay/<n>/State` | the PV contactor, per slot |
| Home Assistant `turn_on`/`turn_off` | the PV contactor instead, where it's on a switch in HA (`pv_switch`). Its state is polled and recorded in the relay's place. It has no fail-safe of its own, so dess-oxide turns PV back on at release, at shutdown, and at startup after stopping with it off |
| `settings/0/Settings/CGwacs/BatteryLife/MinimumSocLimit` | the outage backstop only, restored afterwards |

**Bypass.** With ESS in external control and nothing asked of them, the inverters pass the grid through and the battery idles. They then draw about 20 W instead of about 60 W idling in mode 1 (measured at George's site). The battery model has both, so the planner prefers bypass to trickle-charging or trickle-discharging. The mode is settled once per slot, since it's a stored setting.

## 6. Data and storage

- **Prices:** Nord Pool's data portal (15-minute NL day-ahead, €/MWh). The tariff is applied at planning time.
- **Weather:** Open-Meteo, `knmi_seamless` (KNMI Harmonie-AROME, then ECMWF). History from the historical-forecast archive back to 2024-07-01, so models train on the same kind of forecast they predict from. With it comes the irradiance two more models expect, ECMWF's IFS (`ecmwf_ifs025`) and ICON (`icon_seamless`), for the PV forecast (7.3).
- **Price model inputs:** EnergyZero's hourly EPEX NL prices (history), Open-Meteo's forecasts at points in NL and Germany, and optionally NED's Dutch wind and solar forecasts.
- **Home Assistant:** location and language (REST), hourly energy statistics and OpenAmber's state history (WebSocket), the weather station's states.

SQLite at `/data/dess.db`: slot measurements, prices, weather, observations, plans (every plan, 14 days), efficiency bins, HA hourly statistics, heat pump modes, market prices and inputs, models and settings. Migrations are only ever appended.

## 7. Models

### 7.1 Approach

Small models with physical structure where there is physics (PV, heat pump), plain statistics where that's enough (hot water), and a network or trees where it's habits or markets (base load, prices). Each is trained after a start and nightly, scored on held-out days (every fifth day), and used only while it beats its baseline. The page shows each model's error, its baseline's, and whether it's in use.

Home Assistant's statistics can glitch (a counter reset shows up as its whole total in one hour). Hours where a sensor moved more than 100 kWh, or went down, are left out everywhere; the page counts them per sensor.

### 7.2 Battery and inverters

- **Conversion losses:** from steady-state samples (AC and DC both stable for 20 s, only while ESS regulates), a curve per direction `loss(P) = a + b·P + c·P²` with `b, c ≥ 0`. Until there's enough data (several hundred samples over a 1.5 kW span), a typical MultiPlus-II curve.
- **Standby draw (`a`):** measured on its own, from samples where ESS regulates and the inverters convert next to nothing; until there are five minutes of those, Victron's 20 W per unit. The curves are fitted around it. Fitted together with them, it's poorly pinned down when the controller bypasses the inverters whenever the battery idles, and a wrong standby bends the curves.
- **Bypass draw:** the measured AC draw in bypass.
- **Power limits:** current limits apply at the battery's terminals and become AC power through the loss curves: the BMS's, DVCC's "limit charge current", and each unit's charger rating. The inverters' continuous power and ESS's "limit charge power" and "limit inverter power" apply on the AC side. The lowest wins in each direction. The ratings come from the units' model name ("MultiPlus-II 48/5000/70-50": 70 A, 80 % of 5000 VA). The BMS's limits count as the highest it reported today or yesterday: ESS keeps to a temporary drop every second, but the plan doesn't assume it for two days.
- **Charge taper:** near full the charger holds the absorption voltage and the current falls off. Above 80 % SoC, when ESS doesn't reach its setpoint while charging, the battery is taking less than asked; its power then is what it accepts at that SoC (counted after 20 s, so ESS's ramps don't). Until measured, a typical LFP shape: full power to 95 %, 30 % of it at 100 %. The planner limits charging by it, at the SoC halfway through each move, so it charges earlier instead of counting on full power at the top.
- **Capacity:** the GX device's figure (Dynamic ESS setting or the BMS's installed Ah), then learned from long one-way stretches (ΔSoC ≥ 30 %): `C = ∫P_dc dt / ΔSoC`.
- **The cells' own round trip:** from the BMS's energy in and out counters. Dividing out by in is off by whatever the battery held more or less at the end than at the start. The stored energy has no long-run trend, though, so the ratio of the two counters' trends (least squares over every hour) is the round trip. Used once the counters cover twenty times the battery's swing. Otherwise from the capacity stretches (their ratio), else 96 % for LFP.
- **SoC:** a BMS that reports whole percent (320 Wh steps) is refined by integrating battery power within the reported value's rounding band and re-anchoring where it steps.

### 7.3 PV

**Physics.** Per virtual array: effective kWp (losses folded in), tilt and azimuth, plus the inverter's AC limit as a soft clip, all learned from hourly history against the archived forecast. Configured arrays are the starting point. The virtual arrays needn't match the physical strings; only the inverter's total is seen. Hours with the PV relay open are excluded.

**Correction.** The physics can't know about shading when the sun is low, reflection at shallow angles, or a weather forecast that reads low. On a year of one site's data, the panels made half of what the physics predicted with the sun below 10° and a tenth more than predicted above 30°: one kWp figure has to compromise. So boosted trees are fitted to what the physics got wrong, from the physics' own output, the irradiance, temperature and wind, the sky's clearness, and the sun's height and direction. No date is needed: the sun's position carries the season. On that site it takes about a fifth off the forecast's hourly error and removes the seasonal bias (the physics alone forecast 29 % too much in winter). Tried once there are about two months of daylight hours, and used while it beats the physics alone on held-out days.

**Three weather models.** Most of what's left is the weather forecast: fed the irradiance a satellite measured, a model's error on that site is half of what it is from the forecast. Weather models disagree most about clouds, and none is right every day. Over a year at two Dutch sites, KNMI's day-ahead irradiance was off by 77 W/m² on average against the satellite, ICON's by 65 and ECMWF's by 56, and KNMI was the best of the day on one day in ten. So the correction also gets, for ECMWF and ICON, the physics' output under their sky and their global and direct irradiance, and learns how far to trust each at the site. On the one site's year that took the hourly error for the next day from 0.49 to 0.41 kWh, and for the next hours from 0.46 to 0.34. It needs history of all three, which the archive has back to 2024; where nearly all hours have it the trees are fitted on those, otherwise on KNMI alone. A slot without a value from one of them counts as that model agreeing with KNMI.

Tested and not adopted:

- Training on measured (satellite) irradiance instead of forecasts. The panels are learned more truly that way (a tilt of 36° instead of 15° on the one site), but the forecast for the next day gets worse, 0.63 against 0.53 kWh per hour: the question is what the panels make when the forecast says this, and the forecast's habits are part of the answer.
- Training on the forecasts as they stood a day ahead, rather than the archive's freshest runs. The same accuracy for the next day with one weather model, and 2 % better with three, for a second archive to keep.
- Trees without the physics: nearly as good with a year of data, but nothing to start from at a new site.

### 7.4 Base load

Everything but the heat pump and the EV: a small MLP (burn) on time of day, weekday, Dutch holidays, temperature and daylight. Baseline: the same hour on the same weekday over the four weeks before.

### 7.5 Heat pump and hot water

- **Heating (burn grey-box):** heat from the learned lag of outdoor temperature below a balance point, with wind and sun terms. It's divided by a COP that falls with temperature, times a frost term (the air's water above what a coil a few kelvin colder can hold), plus standby. Only electricity is metered, so the page shows electricity use at given temperatures and never a COP.
- **Hot water:** with OpenAmber, its MAIN state (DHW vs heating) and legionella flag split the meter's energy per quarter hour. Heating is learned from heating alone. Hot water is plain statistics: daily energy against outdoor temperature, the recent hourly profile (which follows the schedule), and legionella runs at the time OpenAmber announces. Without OpenAmber, an hour-of-day term inside the heat pump model.

### 7.6 EV

With `ev.on_input`, the Victron's loads on input are the EV. They're left out of load history and training and aren't forecast. While the car charges, the per-second policy keeps the battery from draining into it unless that pays (§10.1).

### 7.7 Prices not yet published

Gradient-boosted trees (in plain Rust, after EpexPredictor) on:
- wind and sun at points in NL and Germany, which share the market;
- temperature;
- time of day and week, holidays and the sun's position;
- the last two weeks' price level (gas and CO₂);
- optionally NED's Dutch wind and solar forecasts.

Trained on the last half year, nightly. On George's history it scores 1.6 ct/kWh on held-out days (1.5 with NED), against 3.4 for the old estimate (the recent median of the same hour). The model's trees aren't stored: after a restart the old estimate is used until it has retrained, a minute or two later.

### 7.8 Accuracy and money

- Per day: what the last plan before midnight expected for PV, base load, heat pump and house load, against what happened.
- Per lead time (0–6, 6–24, 24–48 h): mean error and bias, from every stored plan.
- Prices: each past slot's price as the last plan before publication expected it, against the published one.
- Money: import at the buy price minus export at the sell price, against the same load and PV without the battery.
- The last week replayed (nightly): dess-oxide's own plans and policy over the recorded loads, PV and prices, against what happened, perfect foresight and no battery, each net of the change in stored energy.

## 8. Tariff

Every component is date-effective.

```
buy  = (spot + markup_buy + energy_tax) × (1 + vat)
sell = (spot + markup_sell + energy_tax) × (1 + vat)   until net_metering_until (salderen ends 2027-01-01)
     = (spot + markup_sell) × (1 + vat if vat_on_export) afterwards
```

The annual netting cap is ignored: it only matters for a net exporter, and net metering ends within months. Fixed costs don't affect decisions.

## 9. Planner

### 9.1 Horizon

From now to at least 48 hours ahead. Published prices first, then the price model's forecast, then the recent median. Estimated slots are marked and shaded on the page. Energy left at the end is valued at 80 % of the horizon's median buy price.

### 9.2 Dynamic programming

- **State:** stored energy on a 100 Wh grid × the PV relay state.
- **Decision per slot:** the next energy level (within the inverters' and BMS's limits) and the relay.
- **Transition:** stored energy → terminal power through the cells' efficiency → AC through the inverters' loss curve. Then `grid = load + AC + standby − PV`, or the bypass draw when the battery holds.
- **Stage cost:** `buy·import − sell·export + wear·|stored energy moved|`, plus penalties below the minimum SoC, above `max_soc` and beyond the grid limits (soft, so a plan always exists).
- **Minimum SoC:** ESS's own "minimum SoC (unless grid fails)". ESS ignores it in a power cut, so what's below it is the backup reserve.
- **Solve:** a backward pass for the value function `V_t(E, relay)`, a forward pass for the plan. About 40 ms for 48 hours.

Why DP rather than a MILP: it's exact for one battery, deterministic, needs no native solver, and handles any efficiency curve. And the whole value function comes for free, which the executor needs.

### 9.3 Cheapest start

For a flexible run (`cheapest_start`: hours, kWh, a night window): each slot's marginal cost of extra load is the per-second policy's cost with and without it, at the planned energy. That's the buy price where the grid would cover it, the value of stored energy where the battery would, and the lost sell price where it cuts an export. The cheapest run is summed over candidates; once its start has come it stays put.

## 10. Executor

### 10.1 Who absorbs forecast errors

The plan is off every slot. "The battery follows the plan and the grid absorbs" (target-SoC control, as Victron's Dynamic ESS does) loses the sell price when a surprise load eats into a planned export. "The grid follows the plan and the battery absorbs" (a fixed setpoint) drains the battery into an EV in a cheap slot. The right answer depends on what stored energy is worth right now, which the plan's value function knows.

### 10.2 The control loop

Every second, for the rest of the slot, it picks the battery power that minimises this slot's cost at the *measured* load and PV plus the plan's cost-to-go for the energy left at the slot's end. Then it writes the resulting grid setpoint (a deadband avoids chasing noise). Exporting at a high price, extra load comes from the battery and the export stays. Holding the battery for a later peak, extra load comes from the grid. SoC drifts from the plan, and the next replan starts from the measured SoC.

### 10.3 Failure behaviour

| Situation | Behaviour |
|---|---|
| Fresh install | Dry run: plan and show, never write. Writing needs **two locks**: `dryrun: false` in the options (missing means a dry run) and the switch on the page. |
| Another controller writes the ESS setpoint (DAO's automations, VRM Dynamic ESS) | Don't take control; release if in control. |
| Switched off, app stopping | Release: the override cleared, ESS back to mode 1, PV on. |
| Stale data, no plan covering now, grid down | Release to plain ESS. |
| Cerbo reboot | The override is gone; the relay boots to PV on. |
| HA host or network down | The last setpoint persists; ESS's minimum SoC and the BMS bound it. |

Manual overrides on the page (hold in bypass, charge, discharge, self-consumption) last until midnight.

## 11. Outage preparedness

For a window set on the page, the plan treats its slots as islanded: PV forced on, import penalised, export worthless. Load is taken 30 % higher and PV 30 % lower. The plan then charges in the cheapest slots beforehand, just enough to get through, with the minimum SoC down to 5 % inside the window. From three hours before until the end, ESS's minimum SoC is raised to the planned SoC at the window's start, so the Victron keeps the reserve even if dess-oxide stops. Grid loss is detected from the active input and shown; control releases to plain ESS, which runs the island.

## 12. Home Assistant integration

### 12.1 The page

Served through ingress only (connections from the Supervisor alone). Server-rendered SVG charts with a small script for hover tooltips (charts over the same hours show the same moment together), legend toggles and a refresh that waits while you're looking. English or Dutch, following HA's language. It shows:
- the plan, and control, overrides and the outage window;
- the last 24 hours against the plan, including prices against their forecasts;
- money, and the last week replayed;
- forecast accuracy;
- the learned models;
- the battery;
- the Victron configuration findings.

### 12.2 Optional entities

With `ha_entities: true`: `sensor.dess_oxide_cheapest_start` (a timestamp for a time trigger) and `binary_sensor.dess_oxide_grid`, set through HA's REST API. A state is sent only when it changes, and every 15 minutes, so HA's history stays small.

## 13. Testing and rollout

- `dess-core`: unit tests and `proptest` properties (the plan respects its bounds and never costs more than idling on the same forecasts), tariff date switches, DST days.
- `dess-models`: each model recovers known parameters from synthetic data.
- `dess-victron`: payload decoding, readings from a real Cerbo's snapshot, and the write capability that only the config grants.
- The executor: noticing another controller, the outage backstop, the relay wiring, overrides within the battery's limits.
- `scripts/check.sh` (fmt, clippy `-D warnings`, tests) runs in CI and before every release.
- **Rollout:** dess-oxide runs as a dry run next to DAO; the page compares forecasts, decisions and cost. Handover:
  1. turn DAO's Victron automations off;
  2. set ESS's stored grid setpoint to about 0–50 W (DAO writes that setting, and plain ESS aims for it);
  3. set `dryrun: false`;
  4. switch control on on the page.

## 14. Still to do

- [ ] Handover at George's site (§13).
- [ ] The brother's install, once his dynamic contract starts.
- [ ] A `backtest` CLI over longer history, and a regression fixture in CI.
- [ ] ENTSO-E as a fallback price source.
- [ ] A reserve from quantile forecasts instead of fixed outage margins.
- [ ] Hot water placed from OpenAmber's schedule window, rather than the learned hourly profile only.

Open questions:
- Export terms from 2027 with each supplier (feed-in fee, VAT on export): they set `markup_sell` and `vat_on_export`.
- Does "HA host or network down" matter enough for a Cerbo-side watchdog that releases the override? Decide after running control for a while.
