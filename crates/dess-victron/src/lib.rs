//! Client for a Victron GX device's local MQTT broker (dbus-flashmq).
//!
//! Venus OS mirrors its D-Bus onto MQTT: `N/<portal>/<service>/<instance>/<path>`
//! carries values as `{"value": …}`. A `R/<portal>/keepalive` read request
//! keeps them flowing. This crate keeps a [`Snapshot`] of every value and
//! turns it into typed readings.
//!
//! The client is **read-only by construction**: the only topics it ever
//! publishes to are `R/…` read requests. Writing (`W/…`) arrives with the
//! executor in a later milestone, as a separate capability.

mod client;
pub mod probe;
pub mod reading;
mod snapshot;
mod value;

pub use client::{Venus, VenusError, VenusOptions};
pub use snapshot::{Entry, Snapshot};
pub use value::Value;
