# dess-oxide

dess-oxide plans a Victron ESS against Dutch 15-minute day-ahead prices.
The design is in
[docs/PLAN.md](https://github.com/georgeboot/dess-oxide/blob/main/docs/PLAN.md).

## What this version does

It runs in **shadow mode**: it plans, records and shows, but **never
writes anything to the Victron system**. It's meant to run next to your
current setup (such as DAO) until you trust its plans.

- **Records** the system once a second from the Cerbo GX's local MQTT. It
  stores 15-minute energy totals and steady-state efficiency samples in
  `/data/dess.db`.
- **Fetches** Nord Pool's 15-minute day-ahead prices, and tomorrow's as soon
  as they're published, around 12:55.
- **Forecasts:**
  - PV from KNMI Harmonie (via Open-Meteo), for your configured arrays at
    Home Assistant's location;
  - load from the recorded history.

  These are simple baselines; learned models come in a later version.
- **Plans** the cheapest battery schedule for the next 48 hours or more, at
  every quarter hour and whenever prices or forecasts change. Every plan is
  stored.
- **Shows** it all on the **dess-oxide** page in the sidebar:
  - the plan;
  - the last 24 hours, comparing what happened with what dess-oxide planned
    and the setpoint your current system actually ran;
  - forecast accuracy.

It creates no Home Assistant entities.

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
  reserve_soc: 0            # kept on top of ESS's minimum SoC
prices:
  area: NL
tariff:                     # each component takes effect on its date
  vat: [{from: "2023-01-01", value: 0.21}]
  energy_tax: [{from: "2026-01-01", value: 0.09161}]
  markup_buy: [{from: "2025-01-01", value: 0.02}]
  markup_sell: [{from: "2025-01-01", value: 0.02}]
  net_metering_until: "2026-12-31"
  net_exporter: false       # true if you export more than you import over the year
  vat_on_export: false
pv:                         # optional: your arrays, for the PV forecast
  - {kwp: 5.59, tilt: 33, azimuth: 193}   # compass degrees, 180 = south
history:                    # optional: HA energy sensors (cumulative kWh)
  grid_import: sensor.p1_meter_energy_import
  grid_export: sensor.p1_meter_energy_export
  pv: sensor.pv_inverter_energy
  battery_in: sensor.battery_ac_charge_energy
  battery_out: sensor.battery_ac_discharge_energy
  heat_pump: sensor.heat_pump_energy
```

**`history`** gives the forecasts real history on day one. dess-oxide
copies these sensors' hourly statistics from Home Assistant: up to three
years at first, then every six hours. House load is derived as
`import − export + pv − battery_in + battery_out`. This only reads from HA.

- **Set your supplier's markups.** The defaults are typical values, not
  yours.
- **`net_exporter`:** if your panels produce more than you use over the
  year, the energy tax isn't at stake on the marginal kWh while net metering
  lasts, so set this to `true`.
