# Changelog

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
