# dess-oxide

dess-oxide will plan and run a Victron ESS against Dutch 15-minute
day-ahead prices. The design is in
[docs/PLAN.md](https://github.com/georgeboot/dess-oxide/blob/main/docs/PLAN.md).

## What this version does

This version only **records**. It never writes anything to the Victron
system. It uses the history it collects to learn efficiency curves, PV
yield and consumption patterns in later versions.

- **Once a second** it reads the system from the Cerbo GX's local MQTT.
- **Every 15 minutes** it stores the energy totals for that slot in
  `/data/dess.db`: grid import and export, PV, loads, battery charge and
  discharge, SoC, relay states and the ESS setpoint.
- **Efficiency samples:** it keeps steady-state inverter/charger
  measurements, which later versions use to learn the conversion efficiency
  curve.
- **At startup** it logs findings about the Victron configuration: things
  that will need to change before dess-oxide can take control.

## Requirements

- A Cerbo GX (or another GX device) on Venus OS 3.50 or newer.
- **MQTT on LAN** enabled on the GX device: Settings → Integrations → MQTT.

## Configuration

```yaml
victron:
  host: 192.168.1.20   # address of the GX device
  port: 1883           # optional
  portal_id: ""        # optional; discovered automatically
```
