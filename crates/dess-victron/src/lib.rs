//! Client for a Victron GX device's local MQTT broker (dbus-flashmq).
//!
//! Venus OS mirrors its D-Bus onto MQTT: `N/<portal>/<service>/<instance>/<path>`
//! carries values as `{"value": …}`. A `R/<portal>/keepalive` read request
//! keeps them flowing. This crate keeps a [`Snapshot`] of every value and
//! turns it into typed readings.
//!
//! Reading is the default. Writing (`W/…`) is a separate capability,
//! [`Writer`], which needs a [`WriteAccess`] that only the `dryrun: false`
//! option creates, and it can only write three values.

mod client;
pub mod probe;
pub mod reading;
mod snapshot;
mod value;
pub mod writer;

pub use client::{Venus, VenusError, VenusOptions};
pub use snapshot::{Entry, Snapshot};
pub use value::Value;
pub use writer::{WriteAccess, Writer};
