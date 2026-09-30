# Changelog

## 0.21.0

- **The battery charges slower near full, and the plan knows it.** Above
  80 % SoC dess-oxide measures how much the battery takes whenever ESS can't
  reach its setpoint while charging (the battery accepting less than asked).
  Until it has seen that, it assumes a typical LFP shape: full power to 95 %,
  falling to 30 % at 100 %. The plan then charges earlier instead of
  counting on full power right up to 100 %. The page shows what it takes at
  95, 97 and 99 %.
- **Loss curves anchored on the measured standby draw.** The inverters'
  draw while ESS regulates and the battery idles is measured on its own, and
  the charge and discharge curves are fitted around it; until measured,
  Victron's 20 W per unit. Fitted together, a controller that bypasses
  whenever the battery idles left too few idle samples, which pulled the
  standby down and flattened the curves.
- **Ratings from the inverters' model name:** the chargers' current and the
  inverters' continuous power come from, for example, "MultiPlus-II
  48/5000/70-50", instead of assuming that model.
- `dess-oxide plan` no longer hangs: it locked its database twice.

## 0.20.3

- **Power limits through the losses.** Current limits (the BMS's, DVCC's,
  the chargers' 70 A each) apply at the battery's terminals. They now become
  AC power through the learned loss curves, instead of a fixed 95 % for
  charging and none for discharging. The inverters' 4 kW each apply on the
  AC side.
- **Limits set on the Cerbo** are followed: ESS's "limit charge power" and
  "limit inverter power", and DVCC's "limit charge current".
- **A BMS that lowers its limits for a while** (a full battery, a cold cell)
  no longer shrinks the whole plan: it plans with the highest limit the BMS
  gave today or yesterday, while ESS keeps to the live limit every second.
- The battery section shows how much power the plan can use, on the AC side
  and at the battery, and what limits each direction.

## 0.20.2

- **Clearer texts around the last 24 hours.** The price chart has its own
  heading and says above it what the two lines are. Below it, it says the
  average miss, or why there's no dashed line yet. The forecast error line
  now reads as plain Dutch and English, and says where the forecast comes
  from.
- The PV forecast error only counts quarter hours with sun; the night's
  zeros made it look better than it was.
- Dashed lines are dashed in the legend and the tooltip too, so the
  forecast and the actual price are told apart.

## 0.20.1

Every field on the page checked against where it comes from. Fixed:

- **Capacity:** the battery section said "usable" for the whole battery.
  It now shows the capacity (0–100 %) and how much of it the plan can use
  above ESS's minimum SoC (and below `max_soc`); what's below the minimum
  stays for a power cut.
- **The cells' round trip** divided the BMS's "out" counter by its "in"
  counter. That's off by however much more or less the battery held at the
  end of the year than at the start, and it counted a sensor's hours even
  where the other had none. It now uses the ratio of the two counters'
  long-run trends over the span both cover, which doesn't care where the
  record starts or ends. The page says what it's measured over, and gives
  the loss per direction and the round trip.
- **"Expected over the horizon"** subtracted the value of all energy left
  in the battery at the end, the reserve included, so it read several euros
  low. Now **"Grid cost ahead"**: what the plan expects to pay the grid, and
  where the battery ends up.
- **Break-even prices** on the stored-energy card now include the wear
  cost, as the planner does.
- **Glitches in Home Assistant's statistics** (a counter reset counted as
  its whole total in one hour) went into the base load and heat pump
  training and the baselines they're compared with. Hours where a sensor
  went down or moved more than 100 kWh are now left out, and the history
  table counts them per sensor.
- **Price model after a restart:** it said "in use" while it was still
  retraining (its trees aren't stored); it now says so until it's back.
- The heat pump's baseline is the same hour's average over the previous
  seven days (the page said "last week's same hours"); both house models
  now show what the held-out hours averaged, so the errors have a scale.
- Hot water no longer says "at 15 °C and warmer" when it found no effect of
  the temperature, and says its daily error is on the days it learned from.
- The current quarter hour says "bypass" when the plan holds the battery.

New:

- **Prices in the last 24 hours:** the published buy price against what the
  last plan before publication forecast, with the mean error. The plan's
  note says where the shaded (unpublished) prices come from.

The design document is now `docs/DESIGN.md`: how dess-oxide works as built,
and the short list of what's left.

## 0.20.0

- **Price forecast:** prices that aren't published yet are now forecast
  from what drives them: wind and sun in NL and Germany, temperature, the
  time of day and week, holidays and the recent price level. A model
  trained nightly on the last half year of EPEX NL prices (EnergyZero) and
  weather forecasts (Open-Meteo), used while it beats the old estimate (the
  recent median) on held-out days. On real data it roughly halves the
  error: 1.6 ct/kWh against 3.4. So the plan knows sooner whether to keep
  energy for a dear morning or evening beyond tomorrow.
- **`ned_api_key`** (optional, free from ned.nl): NED's forecasts of Dutch
  wind and solar production make it better still (about 1.5 ct/kWh).
- `dess-oxide price-backtest` trains the model on recent history and
  reports how it does.

## 0.19.0

- **The battery's own losses count too:** what goes into the cells doesn't
  all come back out. The plan now includes the cells' round trip on top of
  the inverters' losses: measured from the BMS's counters (energy out over
  energy in, over the last year), else from the long charge and discharge
  stretches, else about 4 %. Small price spreads are no longer traded when
  the round trip eats them.

## 0.18.0

- **The reserve is ESS's own minimum SoC** on the GX device ("Minimum SoC
  (unless grid fails)"): kept while the grid is up, used by the inverters
  in a power cut. dess-oxide plans above it; `battery.reserve_soc` is no
  longer used.
- **Expected outages use the reserve:** in the window you set on the page,
  the plan may go down to about 5 %.
- **`battery.max_soc`:** the highest SoC to plan for (default 100 %).
- **The battery card** shows the energy above the reserve, instead of the
  whole capacity as "usable".
- **No `battery.capacity_kwh` anymore:** the capacity comes from the GX
  device (its Dynamic ESS capacity setting, or the BMS's installed Ah) and
  is then learned from long charge and discharge stretches.

## 0.17.0

- **`net_exporter` is gone:** until net metering ends (1 January 2027)
  every exported kWh is netted, energy tax and VAT included. With a
  battery's losses a home is rarely a net exporter over the year, and
  after salderen it doesn't matter anymore. The option is ignored if set.
- **Two tariff registers:** a history sensor option can list several
  sensors separated by commas (a P1 meter's T1 and T2); they're summed.

## 0.16.1

- The price card says why selling pays less than buying when it does
  (set as a net exporter, net metering ended, or different markups).
- The docs explain `net_exporter` better: it's about the meter over the
  year, battery losses and the heat pump included, not panels against use.

## 0.16.0

- **Dutch:** the page follows Home Assistant's language (or set `language`
  to `en` or `nl`), with Dutch day names and decimal commas.
- **Local weather station** (`weather_station`, e.g. an Ecowitt WS90): the
  last hour it measured corrects the forecast for the next hours
  (temperature and humidity fading over a few hours, sunshine within about
  an hour). The page shows the readings and the correction.
- **Clearer "stored energy" card:** "One more kWh in the battery is worth",
  with what it means right now: charging from the grid pays below one
  price, discharging into the grid above another, and in between the
  battery holds.

## 0.15.0

- **Bypass, like DAO's:** when the battery has nothing worthwhile to do in a
  quarter hour, the plan holds it in bypass (ESS external control: the
  battery idle, the grid passing through). The inverters draw less then
  (measured from your Cerbo; about 22 W against 60 W idling), so the plan
  no longer trickle-charges or trickle-discharges. With control on, ESS is
  switched to external control for those quarter hours and back otherwise;
  "hold the battery" uses bypass too. The plan table shows bypass slots.
- The efficiency fit leaves out bypass periods (they made the standby look
  lower); its idle samples start over.
- **Handover check:** with `dryrun: false`, the page warns when ESS's own
  grid setpoint is far from zero (DAO writes that setting, and plain ESS
  aims for it whenever dess-oxide isn't in control).

## 0.14.1

- **Fix:** the "ESS setpoint" line showed the stored setpoint while ESS was
  in external control (mode 3, DAO's bypass), where it means nothing. Now
  there's no line then.
- Control's status says so plainly when ESS is in external control.

## 0.14.0

- **The AC battery options are gone:** `history.battery_in` and
  `battery_out` are no longer read. Give the BMS's DC counters
  (`battery_dc_in`, `battery_dc_out`); dess-oxide works out the AC side
  itself. Without them, the house load comes from its own recordings.

## 0.13.0

- **Battery history from the BMS's DC counters:** set `history.battery_dc_in`
  and `battery_dc_out` (e.g. the BMS's kWh counters) instead of AC sensors.
  Each hour's DC energy is turned into AC with the losses dess-oxide learned
  from the inverters, plus their standby draw. They win over the AC pair.
- **Fix:** without battery sensors, the house load from Home Assistant's
  history left the battery out. Now only dess-oxide's own recordings are
  used then.
- **OpenAmber:** set `openamber_device` (e.g. `openamber`) to split the heat
  pump's energy into heating, hot water and legionella runs, from
  OpenAmber's control loop state and the meter's history. Heating is then
  learned from heating alone, and hot water gets its own forecast: energy
  per day against the outdoor temperature, at the hours it usually runs,
  and legionella runs at the announced time. The page shows the split per
  day. The split history starts with what Home Assistant's recorder still
  has (10 days by default) and grows from there.
- The heat pump model has a separate standby draw.

## 0.12.0

- **Fix: the heat pump model is used on its own.** Before, a heat pump model
  that beat its baseline was only used together with a base-load model, so
  the load forecast fell back to plain history. Now the load is base load
  (the model, or history without the heat pump) plus the heat pump model.
- **Interactive charts:** hover (or tap) for every series' value at that
  time; click a legend entry to hide or show it. The page no longer
  reloads while you're looking at it.
- **Forecast accuracy per day:** what the last plan before midnight expected
  against what happened, for PV, base load, heat pump and the whole house.
  The plan chart also shows the heat pump's share of the load forecast.
- **Honest heat pump figures:** without a heat meter only electricity is
  known, so the page no longer shows a COP or a heat loss. It shows the
  electricity heating takes at 0 °C and −7 °C, the wind and frost effects,
  and hot water per day.
- The PV section explains the fitted inverter limit: with arrays that never
  reach it, it only sits above everything recorded and has no effect.

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
