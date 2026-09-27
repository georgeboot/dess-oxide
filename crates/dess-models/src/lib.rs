//! Learned forecasting models for dess-oxide.
//!
//! Burn fits the models; predictions are plain Rust, so nothing here runs at
//! plan time except arithmetic.

pub mod features;
pub mod heatpump;
pub mod pv;
