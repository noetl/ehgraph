//! The on-disk format marker (clarification E9).
//!
//! Every JSON file ehgraph writes into a data directory or a backup prefix carries
//! `"format": N`. A reader accepts a file only when
//!
//! 1. `format <= STORE_FORMAT` (a file with no `format` field predates the marker and
//!    is format 1), and
//! 2. the file decodes with **no field this build does not know** — the structs read
//!    through here are `#[serde(deny_unknown_fields)]`.
//!
//! # Why both halves
//!
//! An embedded instance rolls back with its host: taking noetl-server back one
//! release takes ehgraph back with it, onto a data directory the newer build wrote.
//! Before this marker the manifest and head files silently ignored fields they did
//! not know, so an older build would load a newer manifest, drop whatever the new
//! fields meant, and write it back without them. That is the EHDB #439 failure:
//! a reader that "falls back" instead of refusing.
//!
//! The marker makes a deliberate format change refuse cleanly with a message that
//! names the file and both formats. `deny_unknown_fields` catches the change that
//! forgot to bump it.
//!
//! # Rule for future changes
//!
//! Any change to a marked file's shape — a new field, even an optional one — bumps
//! [`STORE_FORMAT`]. Readers before the bump then refuse the data directory rather
//! than misreading it. Roll forward, or restore a backup taken by the older build.
//! Change-log frames carry their own per-frame version (`frame.rs`) and are not
//! covered here.

use std::path::Path;

use ehgraph_core::EhError;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The newest on-disk format this build reads and the one it writes.
pub const STORE_FORMAT: u32 = 1;

/// The `format` of a file written before the marker existed.
pub fn legacy_format() -> u32 {
    1
}

/// The value written into every marked file.
pub fn current_format() -> u32 {
    STORE_FORMAT
}

/// Decode a marked JSON file, refusing a newer format or an unknown field.
///
/// `path` appears in every error so an operator can tell which file refused.
pub fn decode_marked<T: DeserializeOwned>(bytes: &[u8], path: &Path) -> Result<T, EhError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| {
        EhError::unavailable(format!(
            "{} is not valid JSON ({err}); the data directory is damaged",
            path.display()
        ))
    })?;
    let format = match value.get("format") {
        None => legacy_format(),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad_marker(path, &value))?,
        Some(_) => return Err(bad_marker(path, &value)),
    };
    if format > STORE_FORMAT {
        return Err(EhError::unavailable(format!(
            "{} is on-disk format {format}, newer than this build reads (format {STORE_FORMAT}). \
             It was written by a newer ehgraph. Refusing to open rather than ignore what this \
             build does not understand (E9). Roll forward, or restore a backup taken by this \
             version",
            path.display()
        )));
    }
    serde_json::from_value(value).map_err(|err| {
        EhError::unavailable(format!(
            "{} (format {format}) has content this build does not understand: {err}. Refusing \
             to open rather than drop it (E9). If a newer ehgraph wrote it without bumping \
             the format, roll forward",
            path.display()
        ))
    })
}

fn bad_marker(path: &Path, value: &Value) -> EhError {
    EhError::unavailable(format!(
        "{} has an unreadable format marker {:?}; refusing to open (E9)",
        path.display(),
        value.get("format")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        #[serde(default = "legacy_format")]
        format: u32,
        a: u64,
    }

    fn decode(text: &str) -> Result<Probe, EhError> {
        decode_marked(text.as_bytes(), Path::new("probe.json"))
    }

    #[test]
    fn unmarked_file_is_format_one() {
        assert_eq!(decode(r#"{"a":1}"#).unwrap(), Probe { format: 1, a: 1 });
    }

    #[test]
    fn current_format_reads() {
        let text = format!(r#"{{"format":{STORE_FORMAT},"a":2}}"#);
        assert_eq!(decode(&text).unwrap().a, 2);
    }

    #[test]
    fn newer_format_refuses_and_names_the_file() {
        let text = format!(r#"{{"format":{},"a":2}}"#, STORE_FORMAT + 1);
        let err = decode(&text).unwrap_err();
        assert!(err.message.contains("probe.json"), "{}", err.message);
        assert!(
            err.message.contains("newer than this build"),
            "{}",
            err.message
        );
    }

    #[test]
    fn unknown_field_refuses_even_at_the_current_format() {
        let err = decode(r#"{"format":1,"a":2,"added_later":true}"#).unwrap_err();
        assert!(
            err.message.contains("does not understand"),
            "{}",
            err.message
        );
        assert!(err.message.contains("added_later"), "{}", err.message);
    }

    #[test]
    fn non_numeric_marker_refuses() {
        assert!(decode(r#"{"format":"2","a":2}"#).is_err());
        assert!(decode(r#"{"format":-1,"a":2}"#).is_err());
    }

    #[test]
    fn invalid_json_is_damage_not_a_format_question() {
        let err = decode("{\"a\":").unwrap_err();
        assert!(err.message.contains("not valid JSON"), "{}", err.message);
    }
}
