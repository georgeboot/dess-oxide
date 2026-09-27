//! Pure domain logic for dess-oxide.
//!
//! Nothing in this crate does I/O or reads the clock: every function is a
//! deterministic function of its inputs, so the same code runs live and in
//! backtests.

pub mod battery;
pub mod calendar;
pub mod capacity;
pub mod control;
pub mod efficiency;
pub mod forecast;
pub mod heat_pump_modes;
pub mod planner;
pub mod prices;
pub mod record;
pub mod replay;
pub mod slot;
pub mod soc;
pub mod solar;
pub mod tariff;
pub mod units;
pub mod weather;
pub mod weather_correction;

pub use slot::Slot;
pub use units::{EurPerKwh, WattHours, Watts};
