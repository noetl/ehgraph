use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::EhError;

/// Typed property value.
///
/// JSON numbers without a fraction are integers. Nested maps and lists are allowed.
/// A timestamp is a one-key object `{"$timestamp":"<rfc3339>"}` or
/// `{"$timestamp": <unix millis>}`.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Timestamp(i64),
    List(Vec<PropValue>),
    Map(BTreeMap<String, PropValue>),
}

impl PropValue {
    pub fn from_json(value: &Value) -> Result<Self, EhError> {
        match value {
            Value::Null => Err(EhError::invalid("property values must not be null")),
            Value::Bool(b) => Ok(Self::Bool(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Self::Int(i))
                } else if let Some(f) = n.as_f64() {
                    if !f.is_finite() {
                        return Err(EhError::invalid("property floats must be finite"));
                    }
                    Ok(Self::Float(f))
                } else {
                    Err(EhError::invalid("unsupported JSON number"))
                }
            }
            Value::String(s) => Ok(Self::String(s.clone())),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(Self::from_json(item)?);
                }
                Ok(Self::List(out))
            }
            Value::Object(map) => {
                if map.len() == 1 {
                    if let Some(ts) = map.get("$timestamp") {
                        return parse_timestamp(ts);
                    }
                }
                let mut out = BTreeMap::new();
                for (k, v) in map {
                    out.insert(k.clone(), Self::from_json(v)?);
                }
                Ok(Self::Map(out))
            }
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::String(s) => Value::String(s.clone()),
            Self::Int(i) => Value::Number((*i).into()),
            Self::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Self::Bool(b) => Value::Bool(*b),
            Self::Timestamp(ms) => {
                let dt = DateTime::<Utc>::from_timestamp_millis(*ms)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| ms.to_string());
                serde_json::json!({"$timestamp": dt})
            }
            Self::List(items) => Value::Array(items.iter().map(Self::to_json).collect()),
            Self::Map(map) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in map {
                    obj.insert(k.clone(), v.to_json());
                }
                Value::Object(obj)
            }
        }
    }

    pub fn cmp_num(&self, other: &PropValue) -> Option<std::cmp::Ordering> {
        let left = self.as_f64()?;
        let right = other.as_f64()?;
        left.partial_cmp(&right)
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            Self::Timestamp(ms) => Some(*ms as f64),
            _ => None,
        }
    }
}

fn parse_timestamp(value: &Value) -> Result<PropValue, EhError> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .map(PropValue::Timestamp)
            .ok_or_else(|| EhError::invalid("timestamp millis must be an integer")),
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| PropValue::Timestamp(dt.timestamp_millis()))
            .map_err(|err| EhError::invalid(format!("bad timestamp: {err}"))),
        _ => Err(EhError::invalid(
            "$timestamp must be an RFC3339 string or unix millis",
        )),
    }
}

impl Serialize for PropValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PropValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        PropValue::from_json(&value).map_err(serde::de::Error::custom)
    }
}

/// Equality or a numeric range. A bare JSON value means equality.
#[derive(Debug, Clone, PartialEq)]
pub enum PropPredicate {
    Eq(PropValue),
    Range {
        gte: Option<PropValue>,
        gt: Option<PropValue>,
        lte: Option<PropValue>,
        lt: Option<PropValue>,
    },
}

impl PropPredicate {
    pub fn matches(&self, value: &PropValue) -> bool {
        match self {
            Self::Eq(expected) => value == expected,
            Self::Range { gte, gt, lte, lt } => {
                if let Some(bound) = gte {
                    match value.cmp_num(bound) {
                        Some(std::cmp::Ordering::Less) | None => return false,
                        _ => {}
                    }
                }
                if let Some(bound) = gt {
                    match value.cmp_num(bound) {
                        Some(std::cmp::Ordering::Greater) => {}
                        _ => return false,
                    }
                }
                if let Some(bound) = lte {
                    match value.cmp_num(bound) {
                        Some(std::cmp::Ordering::Greater) | None => return false,
                        _ => {}
                    }
                }
                if let Some(bound) = lt {
                    match value.cmp_num(bound) {
                        Some(std::cmp::Ordering::Less) => {}
                        _ => return false,
                    }
                }
                true
            }
        }
    }

    pub fn from_json(value: &Value) -> Result<Self, EhError> {
        if let Value::Object(map) = value {
            const KEYS: [&str; 5] = ["eq", "gte", "gt", "lte", "lt"];
            let is_range = map.len() <= KEYS.len()
                && !map.is_empty()
                && map.keys().all(|k| KEYS.contains(&k.as_str()));
            if is_range {
                let one = |key: &str| -> Result<Option<PropValue>, EhError> {
                    match map.get(key) {
                        Some(v) => Ok(Some(PropValue::from_json(v)?)),
                        None => Ok(None),
                    }
                };
                return Ok(Self::Range {
                    gte: one("gte")?,
                    gt: one("gt")?,
                    lte: one("lte")?,
                    lt: one("lt")?,
                });
            }
        }
        Ok(Self::Eq(PropValue::from_json(value)?))
    }
}

impl Serialize for PropPredicate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Eq(v) => v.serialize(serializer),
            Self::Range { gte, gt, lte, lt } => {
                let mut map = serde_json::Map::new();
                if let Some(v) = gte {
                    map.insert("gte".into(), v.to_json());
                }
                if let Some(v) = gt {
                    map.insert("gt".into(), v.to_json());
                }
                if let Some(v) = lte {
                    map.insert("lte".into(), v.to_json());
                }
                if let Some(v) = lt {
                    map.insert("lt".into(), v.to_json());
                }
                Value::Object(map).serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for PropPredicate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        PropPredicate::from_json(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_float_timestamp_and_range() {
        let value = PropValue::from_json(&serde_json::json!(1500)).unwrap();
        assert_eq!(value, PropValue::Int(1500));
        let pred = PropPredicate::from_json(&serde_json::json!({"gte": 1500, "lt": 9000})).unwrap();
        assert!(pred.matches(&value));
        assert!(!pred.matches(&PropValue::Int(9000)));
        let ts = PropValue::from_json(&serde_json::json!({"$timestamp": "2026-11-03T00:00:00Z"}))
            .unwrap();
        assert!(matches!(ts, PropValue::Timestamp(_)));
    }

    #[test]
    fn nested_map_roundtrip() {
        let raw = serde_json::json!({"site": "s1", "tags": ["a", "b"]});
        let parsed = PropValue::from_json(&raw).unwrap();
        assert_eq!(parsed.to_json(), raw);
    }
}
