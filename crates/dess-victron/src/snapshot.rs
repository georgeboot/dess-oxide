use std::collections::HashMap;

use jiff::Timestamp;

use crate::value::Value;

/// The latest value of one topic.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub value: Value,
    /// When we last received it.
    pub received: Timestamp,
    /// How many times it changed while we were listening.
    pub changes: u32,
}

/// Every value seen on the broker, keyed by `<service>/<instance>/<path>`,
/// e.g. `system/0/Dc/Battery/Soc`.
///
/// dbus-flashmq only publishes changes, so a value's age says nothing about
/// whether it is current. Liveness comes from the `heartbeat` topic instead.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    entries: HashMap<String, Entry>,
}

impl Snapshot {
    pub fn update(&mut self, key: &str, value: Value, received: Timestamp) {
        match self.entries.get_mut(key) {
            Some(entry) => {
                if entry.value != value {
                    entry.changes += 1;
                    entry.value = value;
                }
                entry.received = received;
            }
            None => {
                self.entries.insert(
                    key.to_owned(),
                    Entry {
                        value,
                        received,
                        changes: 0,
                    },
                );
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|e| e.value.as_f64())
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|e| e.value.as_str())
    }

    pub fn changes(&self, key: &str) -> u32 {
        self.get(key).map_or(0, |e| e.changes)
    }

    /// When the GX device last sent its heartbeat (every few seconds).
    pub fn last_heartbeat(&self) -> Option<Timestamp> {
        self.get("heartbeat").map(|e| e.received)
    }

    /// Device instances present for a service type, e.g. `pvinverter` → `[31]`.
    pub fn instances(&self, service: &str) -> Vec<u32> {
        let mut instances: Vec<u32> = self
            .entries
            .keys()
            .filter_map(|key| {
                let mut parts = key.splitn(3, '/');
                (parts.next()? == service).then_some(())?;
                parts.next()?.parse().ok()
            })
            .collect();
        instances.sort_unstable();
        instances.dedup();
        instances
    }

    /// Entries whose key starts with `prefix`, sorted by key.
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> Vec<(&'a str, &'a Entry)> {
        let mut found: Vec<_> = self
            .entries
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, entry)| (key.as_str(), entry))
            .collect();
        found.sort_by_key(|(key, _)| *key);
        found
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_changes_and_lists_instances() {
        let t = Timestamp::UNIX_EPOCH;
        let mut s = Snapshot::default();
        s.update("pvinverter/31/Ac/Power", Value::Number(1.0), t);
        s.update("pvinverter/31/Ac/Power", Value::Number(1.0), t);
        s.update("pvinverter/31/Ac/Power", Value::Number(2.0), t);
        s.update("pvinverter/40/Ac/Power", Value::Null, t);
        s.update("pvinverterx/7/Ac/Power", Value::Null, t);
        assert_eq!(s.changes("pvinverter/31/Ac/Power"), 1);
        assert_eq!(s.number("pvinverter/31/Ac/Power"), Some(2.0));
        assert_eq!(s.instances("pvinverter"), vec![31, 40]);
    }
}
