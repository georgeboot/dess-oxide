# dess-oxide

dess-oxide plans a Victron ESS against Dutch 15-minute day-ahead prices.
How it works is in
[docs/DESIGN.md](https://github.com/georgeboot/dess-oxide/blob/main/docs/DESIGN.md).

## What this version does

By default it runs as a **dry run**: it plans, records and shows, but
**never writes anything to the Victron system**. It's meant to run next to
your current setup (such as DAO) until you trust its plans.

**Taking control** needs two locks:
1. `dryrun: false` in the options. It's a dry run when the option is
   missing or `true`.
2. The switch on the dess-oxide page.

It then moves ESS's grid setpoint once a second (a volatile override, not
the stored setting) and switches the PV relay per quarter hour. When the
battery has nothing worthwhile to do in a quarter hour, it puts ESS in
**bypass** instead (external control: the battery idle, the grid passing
through), as DAO does: the inverters then draw less than when ESS idles,
so the plan doesn't trickle-charge or trickle-discharge. It releases control
back to plain ESS (regulating the grid again) whenever:
- the switch goes off;
- the Victron Dynamic ESS is enabled;
- something else wrote the ESS setpoint in the last five minutes (turn
  DAO's automations off first);
- data is stale;
- the grid is down;
- the app stops.

While in control, the page can override the plan until midnight: hold the
battery (in bypass), charge, discharge, or plain self-consumption.

**Handing over from DAO:** turn DAO's Victron automations off, and check
ESS's own grid setpoint on the Cerbo (Settings → ESS → Grid setpoint). DAO
writes that setting, and plain ESS aims for it whenever dess-oxide isn't in
control: set it to about 0–50 W. Then set `dryrun: false` and switch control
on here.

- **Records** the system once a second from the Cerbo GX's local MQTT. It
  stores 15-minute energy totals and steady-state efficiency samples in
  `/data/dess.db`.
- **Fetches** Nord Pool's 15-minute day-ahead prices, and tomorrow's as soon
  as they're published, around 12:55.
- **Forecasts:**
  - PV from KNMI Harmonie (via Open-Meteo), at Home Assistant's location,
    through a **learned PV model**. It's trained nightly on your history and
    the archived weather, and starts from your configured arrays. After
    about two months it also learns what the physics can't know, such as
    shading when the sun is low, from what it got wrong.
  - house load from a **learned base-load model plus a learned heat pump
    model**. The heat pump model includes frost losses in humid air. Until
    they beat the naive forecast, load comes from history.
- **Learns the battery**: the inverters' losses and standby draw, the
  capacity from long charge and discharge stretches, and the cells' own
  round trip from the BMS's energy counters. It also keeps a finer state of
  charge than a BMS that reports whole percent.
- **Keeps a backup reserve**: the plan never goes below ESS's own "minimum
  SoC (unless grid fails)", set on the Cerbo under Settings → ESS. ESS
  ignores that minimum in a power cut, so what's below it is there for
  backup. `battery.max_soc` caps the other end.
- **Plans** the cheapest battery schedule for the next 48 hours or more, at
  every quarter hour and whenever prices or forecasts change. Every plan is
  stored.
- **Prepares for a power cut** you expect: set the window on the page. The
  plan then charges in the cheapest slots beforehand to cover the window's
  load (with margins: load +30 %, PV −30 %), and keeps PV on during it. With
  control on, ESS's own minimum SoC is raised to that reserve from three
  hours before the window until its end, so the Victron keeps it even if
  dess-oxide stops. The original minimum is restored afterwards.
- **Shows** it all on the **dess-oxide** page in the sidebar:
  - the plan;
  - the last 24 hours, comparing what happened with what dess-oxide planned
    and the setpoint your current system actually ran, and the prices with
    what was forecast for them before they were published;
  - forecast accuracy over the last week, per day and by how far ahead each
    forecast was made;
  - what today, yesterday, the last week and this month cost, and what
    they would have cost without the battery. While DAO runs the system,
    that's DAO's result, so it's the yardstick for the handover;
  - the last week replayed: dess-oxide's own plans and policy run over the
    same loads, PV and prices, next to what actually happened, perfect
    foresight and no battery (updated nightly).

It creates no Home Assistant entities unless you turn on `ha_entities`.

## Requirements

- A Cerbo GX (or another GX device) on Venus OS 3.50 or newer.
- **MQTT on LAN** enabled on the GX device: Settings → Integrations → MQTT.

## Configuration

Units are kW, kWh, €/kWh excluding VAT, and degrees.

```yaml
victron:
  host: 192.168.1.20        # address of the GX device
  pv_relay: 2               # optional: Cerbo relay driving a PV contactor
  pv_relay_energized: pv_off
grid:
  max_import_kw: 17
  max_export_kw: 17
battery:
  wear_cost_eur_per_kwh: 0
  max_soc: 100              # the highest SoC to plan for
prices:
  area: NL
tariff:                     # each component takes effect on its date
  vat: [{from: "2023-01-01", value: 0.21}]
  energy_tax: [{from: "2026-01-01", value: 0.09161}]
  markup_buy: [{from: "2025-01-01", value: 0.02}]
  markup_sell: [{from: "2025-01-01", value: 0.02}]
  net_metering_until: "2026-12-31"
  vat_on_export: false
pv:                         # optional: your arrays, for the PV forecast
  - {kwp: 5.59, tilt: 33, azimuth: 193}   # compass degrees, 180 = south
history:                    # optional: HA energy sensors (cumulative kWh)
  grid_import: sensor.p1_meter_energy_import
  grid_export: sensor.p1_meter_energy_export
  pv: sensor.pv_inverter_energy
  battery_dc_in: sensor.bms_energy_in     # the BMS's counters (DC)
  battery_dc_out: sensor.bms_energy_out
  heat_pump: sensor.heat_pump_energy
  ev: sensor.ev_charger_energy   # optional: left out of the house load
cheapest_start:             # a flexible run, such as the dishwasher
  hours: 3
  kwh: 1
  earliest: "20:00"
  finish_by: "08:00"
ha_entities: false          # publish a few entities for automations
ev:
  on_input: false           # the charger is between the grid meter and the Victrons
openamber_device: ""        # e.g. openamber: split the heat pump into heating and hot water
pv_switch:                  # optional: the PV on a Home Assistant switch instead of the GX relay
  entity: switch.pv_contactor   # several, one per inverter: separate with commas
  on_means: pv_on               # or pv_off, if switching it on disconnects the PV
weather_station:            # optional: a local station in HA, such as an Ecowitt WS90
  temperature: sensor.ws90_outdoor_temperature
  humidity: sensor.ws90_humidity
  wind_speed: sensor.ws90_wind_speed
  solar_radiation: sensor.ws90_solar_radiation
language: auto              # the page's language: auto (Home Assistant's), en or nl
ned_api_key: ""             # optional: a free key from ned.nl, for better price forecasts
```

**`history`** gives the forecasts real history on day one. dess-oxide
copies these sensors' hourly statistics from Home Assistant: up to three
years at first, then every six hours. House load is derived as
`import − export + pv −` the inverters' AC in `+` their AC out. This only
reads from HA. Hours where a sensor went down or jumped more than 100 kWh
(a counter reset in HA's statistics) are left out; the page counts them per
sensor under "History imported from Home Assistant".

A meter that reports two tariff registers (T1 and T2, as Dutch P1 meters
do) can be given as one comma-separated line, e.g.
`grid_import: sensor.meter_import_t1, sensor.meter_import_t2`: they're
summed. The same works for `grid_export`, `pv`, the battery counters and
`ev`.

For the battery, give the BMS's DC counters (`battery_dc_in`,
`battery_dc_out`): dess-oxide turns each hour's DC energy into AC itself,
with the losses it learns from the inverters and their standby draw. No AC
sensor is needed. Without the counters, the house load comes from
dess-oxide's own recordings only (the models then need a couple of weeks).

**`cheapest_start`** is DAO's `machines`, simplified: the page shows when
to start a run of `hours` using `kwh` so it's cheapest, within the night
window. "Cheapest" is the plan's marginal cost of the extra load, so it
counts what the battery and PV would otherwise do, not just the spot price.
Once the start time has come, it stays put until the window closes.

**`ha_entities: true`** publishes two entities, and nothing else:
- `sensor.dess_oxide_cheapest_start`, a timestamp: use it as a time trigger.
- `binary_sensor.dess_oxide_grid`: off while the grid is down.

They change only when their value does, so they add next to nothing to
HA's history. For example:

```yaml
automation:
  - alias: Dishwasher at the cheapest time
    trigger:
      - platform: time
        at: sensor.dess_oxide_cheapest_start
    action:
      - action: button.press   # whatever starts your dishwasher remotely
        target:
          entity_id: button.dishwasher_start
```

**`openamber_device`**: with an OpenAmber heat pump controller (and
`history.heat_pump` set), its ESPHome device name, e.g. `openamber`.
dess-oxide then reads OpenAmber's control loop state (MAIN), its legionella
flag and the heat pump meter from Home Assistant's history, and splits the
heat pump's energy into heating and hot water per quarter hour. The recorder
keeps states for 10 days by default; dess-oxide keeps its own copy, so this
history grows from the day you set it. Heating is then learned from heating
alone. Hot water gets its own forecast: energy per day against the outdoor
temperature, at the hours it usually runs (your schedule), and legionella
runs at the time OpenAmber announces. The page shows the split per day.

**Switching the PV off** (when feeding in would cost money) works through
either the GX device's relay (`victron.pv_relay`) or a switch in Home
Assistant (`pv_switch`), such as a Shelly on a contactor. Give one of the
two. With `pv_switch`, dess-oxide reads the switch every few seconds, so it
knows when the PV is off (those quarter hours don't count as "the sun
didn't shine"), and it operates the switch only with both locks on. A GX
relay wired fail-safe falls back to PV on by itself; a switch doesn't. So
dess-oxide turns the PV back on when it releases control or stops, and when
it starts up after stopping with the PV off. If Home Assistant can't reach
the switch at that moment, the PV stays off until you switch it on.

**`weather_station`**: the forecast (KNMI's Harmonie model, at your
location) is right on average but can be off on the day: fog it missed, a
colder night, clouds an hour early. With a station's entities, dess-oxide
compares the last hour it measured with the forecast and corrects the next
hours: temperature and humidity by the difference, fading out over a few
hours; sunshine by the ratio (against the station's usual ratio, so a
sensor that reads high doesn't skew PV), fading within about an hour. The
recent past gets the measured temperatures, so the heat pump model's
thermal lag starts from what happened. Wind isn't corrected: a station a
few metres up doesn't measure the 10 m wind the models use. Every sensor is
optional; units are converted.

**Prices not yet published** (beyond tomorrow, or tomorrow before about
13:00) are forecast by a model of what drives them: wind and sun in the
Netherlands and Germany, temperature, the time of day and week, holidays,
and the recent price level. It learns from the last half year of EPEX NL
prices (from EnergyZero) and Open-Meteo's weather forecasts, retrains
nightly, and is only used while it beats the old estimate (the recent
median of the same hour) on held-out days. On the page, the plan's shaded
part is where prices are forecast, and the last 24 hours show how far off
the forecast was. On half a year of history it
roughly halves the error: 1.6 ct/kWh against 3.4. With **`ned_api_key`**
(free from [ned.nl](https://ned.nl/nl/handleiding-api)) it also uses NED's
forecasts of Dutch wind and solar production, which brings it to about
1.5 ct/kWh.

**`ev.on_input`**: if the charger sits between the grid meter and the
Victrons, what the Victron sees as loads on its input is the EV. It's then
left out of the house load's history and forecast. The EV isn't forecast;
while it charges, the per-second control keeps the battery from draining
into the car unless that pays. Set `history.ev` to leave it out of the
imported history too.

- **Set your supplier's markups.** The defaults are typical values, not
  yours.
- **Net metering:** until `net_metering_until` (salderen ends on
  1 January 2027), every exported kWh cancels an imported one, energy tax
  and VAT included, so selling pays as much as buying (with equal
  markups). After that, exports earn the spot price plus `markup_sell`, with
  VAT only if `vat_on_export`.