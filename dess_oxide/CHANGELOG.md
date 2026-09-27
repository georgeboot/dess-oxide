# Changelog

## 0.11.0

- **`control` is now `dryrun`**, and inverted: dess-oxide never writes to
  the Victron while `dryrun` is `true`, which is also what a missing option
  means. To let it take control, set `dryrun: false` and switch control on
  on its page. The old `control` option is ignored.
- The page shows how far back each Home Assistant history sensor goes, so
  it's clear what the models were trained on.

## 0.10.0

- **The last week, replayed:** every night dess-oxide re-runs its own
  plans (with the forecasts it had at the time) and its policy over the
  week's measured load, PV and prices, and shows the cost next to what
  actually happened (DAO, while it's in control), perfect foresight, and
  no battery. All net of the change in stored energy. This is the
  comparison to decide the handover on.

## 0.9.0

- **Money:** what today, yesterday, the last 7 days and this month cost,
  and what they would have cost without the battery. While DAO is in
  control, that's DAO's result: compare it with dess-oxide's after the
  handover.
- **Manual override** (with control on): hold the battery, charge,
  discharge or plain self-consumption, until midnight.

## 0.8.0

- **Forecast accuracy by lead time:** the page compares the last week's
  load and PV forecasts with what happened, split by how far ahead they
  were made (0–6 h, 6–24 h, 24–48 h): error and bias. Useful while it runs
  next to DAO.
- **Finer state of charge:** a BMS that reports whole percent (320 Wh
  steps on 32 kWh) is refined from battery power between its steps, and
  re-anchored at each step. The plan, control and records use it.
- **Learned usable capacity** from long charge and discharge stretches,
  shown with the cells' own round-trip efficiency. A configured
  `battery.capacity_kwh` still wins.

## 0.7.0

- **Cheapest start** for the dishwasher (or any flexible run): the page
  shows when to start it tonight so it costs least. It counts what the
  battery and PV would otherwise do, not just the price. Set the run and
  its night window under `cheapest_start`.
- **Optional Home Assistant entities** (`ha_entities: true`):
  `sensor.dess_oxide_cheapest_start`, for an automation's time trigger, and
  `binary_sensor.dess_oxide_grid`. Off by default.
- **EV awareness** (`ev.on_input`): a charger between the grid meter and the
  Victrons is left out of the house load's history and forecast; while it
  charges, the plan and control see it.
- `probe` reports what it got over slow links instead of giving up.

## 0.6.0

- **Outage mode:** tell dess-oxide when you expect a power cut (start time
  and hours) on its page. The plan charges in the cheapest slots beforehand
  to cover the window's load with margins, and keeps PV on during it. It
  replans right away when you set or cancel the window.
- **Minimum SoC backstop:** with control on, ESS's own minimum SoC is raised
  to the reserve from three hours before the window until its end, so the
  Victron keeps the reserve even if dess-oxide stops. The original minimum
  is restored afterwards.

## 0.5.0

- **Heat pump model:** learns your heat pump's electricity use from its meter
  (`history.heat_pump`) and the archived weather:
  - the temperature where heating stops, and your house's thermal lag;
  - wind and solar effects, and the COP falling with the cold;
  - **frost losses in humid air near freezing**, which is why a foggy 0 °C
    day can cost more than a clear −8 °C one;
  - your hot water times.
- **Base load model:** a small neural network for everything else in the
  house, from time of day, weekday, Dutch public holidays, temperature and
  daylight.
- **Load forecast = base load + heat pump**, used once both models beat their
  naive baselines on held-out days.
- **Battery and inverter losses** are now learned from the recorded
  steady-state samples (standby draw plus a charge and a discharge curve),
  replacing the prior as soon as there's enough data.
- The dess-oxide page shows everything that was learned.
- **Control** (off by default). Set `control: true` and switch it on on the
  page. Once a second it applies the plan's policy to the measured load and
  PV: surprises go to the battery or the grid, whichever is cheaper. It moves
  the volatile setpoint override and the PV relay, and releases control to
  plain ESS on any doubt, including while something else (DAO) still writes
  the setpoint.

## 0.4.0

- **Learned PV model:** dess-oxide trains a PV model on your history (HA
  statistics and its own recordings) and the archived weather. It learns
  effective kWp, tilt and azimuth per array, plus your inverter's output
  limit, starting from the arrays you configured. It trains shortly after
  startup and then nightly. It's only used when it beats the configured
  arrays on held-out data. The dess-oxide page shows what it learned.
- **Unknown options** are now logged and ignored instead of stopping the app.

## 0.3.0

- **History import:** configure `history` with your HA energy sensors. dess-oxide
  copies their hourly statistics (up to three years back), so the load forecast
  has real history from day one. Only reads from HA.
- **Weather:** the KNMI Harmonie forecast (15 minutes) is stored hourly. The
  archive of past forecasts back to July 2024 is copied for training the
  learned models.
- **PV forecast** now uses dess-oxide's own solar geometry (sun position,
  Hay–Davies transposition, cell temperature) on your configured arrays.

## 0.2.0

- **Shadow planning:**
  - fetches Nord Pool's 15-minute prices;
  - applies your tariff (including `net_exporter` for the net-metering year);
  - plans the battery for 48+ hours every quarter hour.

  It never writes to the Victron.
- **The dess-oxide page** in the sidebar:
  - the plan;
  - the last 24 hours, comparing measured, planned and the setpoint that ran
    (DAO's);
  - forecast errors.

## 0.1.0

- **Read-only recorder:** 15-minute energy totals and efficiency samples from
  the Cerbo's local MQTT.
