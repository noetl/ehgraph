use sha2::{Digest, Sha256};

use crate::EhError;

/// Consumer-supplied element ids: non-empty, no control characters, bounded length.
pub fn validate_id(id: &str) -> Result<(), EhError> {
    if id.is_empty() || id.len() > 512 {
        return Err(EhError::invalid(
            "element id must be 1..=512 bytes and non-empty",
        ));
    }
    if id.chars().any(|c| c.is_control() || c == '\n' || c == '\r') {
        return Err(EhError::invalid(
            "element id must not contain control characters",
        ));
    }
    Ok(())
}

pub fn validate_namespace(name: &str) -> Result<(), EhError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(EhError::invalid(
            "namespace must match [A-Za-z][A-Za-z0-9_-]{0,127}",
        ))
    }
}

pub fn validate_owner(owner: &str) -> Result<(), EhError> {
    if owner.is_empty() || owner.len() > 256 || owner.chars().any(|c| c.is_control()) {
        return Err(EhError::invalid(
            "owner must be 1..=256 characters without control characters",
        ));
    }
    Ok(())
}

/// Deterministic edge id when the caller omits one.
///
/// The id is the readable tuple `(src, type, dst, origin)`, not a cryptographic hash.
/// It is stable for the same endpoints, which keeps upserts idempotent.
pub fn default_edge_id(src: &str, type_name: &str, dst: &str, origin: Option<&str>) -> String {
    format!("edge:{src}:{type_name}:{dst}:{}", origin.unwrap_or(""))
}

/// Stable SHA-256 hex of a JSON value. Used for idempotency body comparison.
pub fn canonical_hash(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_id_is_stable() {
        let a = default_edge_id("device:sw1", "CONNECTED_TO", "device:sw2", None);
        let b = default_edge_id("device:sw1", "CONNECTED_TO", "device:sw2", None);
        assert_eq!(a, b);
        assert_ne!(
            a,
            default_edge_id("device:sw1", "CONNECTED_TO", "device:sw2", Some("designed"))
        );
    }

    #[test]
    fn namespace_rules() {
        assert!(validate_namespace("qnetmon").is_ok());
        assert!(validate_namespace("1bad").is_err());
        assert!(validate_namespace("has space").is_err());
    }
}
