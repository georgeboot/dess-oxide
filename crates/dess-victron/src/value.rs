use serde::Serialize;

/// A value published by dbus-flashmq.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    /// The D-Bus value is invalid (`{"value": null}`), e.g. a phase that doesn't exist.
    Null,
    /// The device disappeared from the D-Bus (empty payload).
    Gone,
    Number(f64),
    Text(String),
    /// Arrays, objects and booleans, kept as-is.
    Json(serde_json::Value),
}

impl Value {
    /// Parses a notification payload.
    pub fn parse(payload: &[u8]) -> Self {
        if payload.is_empty() {
            return Self::Gone;
        }
        let Ok(serde_json::Value::Object(mut map)) = serde_json::from_slice(payload) else {
            return Self::Text(String::from_utf8_lossy(payload).into_owned());
        };
        match map.remove("value") {
            None | Some(serde_json::Value::Null) => Self::Null,
            Some(serde_json::Value::Number(n)) => n.as_f64().map_or(Self::Null, Self::Number),
            Some(serde_json::Value::String(s)) => Self::Text(s),
            Some(other) => Self::Json(other),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Json(serde_json::Value::Bool(b)) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_payload_shapes() {
        assert_eq!(Value::parse(br#"{"value": 936}"#), Value::Number(936.0));
        assert_eq!(Value::parse(br#"{"value": 52.96}"#), Value::Number(52.96));
        assert_eq!(Value::parse(br#"{"value": null}"#), Value::Null);
        assert_eq!(Value::parse(b""), Value::Gone);
        assert_eq!(
            Value::parse(br#"{"value": "v3.66"}"#),
            Value::Text("v3.66".into())
        );
        assert_eq!(
            Value::parse(br#"{"max": 1, "min": 0, "value": 1}"#),
            Value::Number(1.0)
        );
        assert_eq!(Value::parse(br#"{"value": true}"#).as_f64(), Some(1.0));
    }
}
