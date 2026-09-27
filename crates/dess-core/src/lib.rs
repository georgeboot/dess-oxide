//! Pure domain logic for dess-oxide.
//!
//! Nothing in this crate does I/O or reads the clock: every function is a
//! deterministic function of its inputs, so the same code runs live and in
//! backtests.

pub mod battery;
pub mod efficiency;
pub mod forecast;
pub mod planner;
pub mod prices;
pub mod record;
pub mod slot;
pub mod tariff;
pub mod units;

pub use slot::Slot;
pub use units::{EurPerKwh, WattHours, Watts};
