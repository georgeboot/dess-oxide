//! Pure domain logic for dess-oxide.
//!
//! Nothing in this crate does I/O or reads the clock: every function is a
//! deterministic function of its inputs, so the same code runs live and in
//! backtests.

pub mod efficiency;
pub mod record;
pub mod slot;
pub mod units;

pub use slot::Slot;
pub use units::{WattHours, Watts};
