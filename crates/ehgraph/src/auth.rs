use std::collections::BTreeSet;

use ehgraph_api::Caller;
use ehgraph_core::EhError;

use crate::config::{AuthConfig, Grant};

/// Resolve an `Authorization` header value (`Bearer <token>`) to a caller.
pub fn authenticate_header(auth: &AuthConfig, header: Option<&str>) -> Result<Caller, EhError> {
    if auth.mode == "disabled" {
        return Ok(Caller::full("anonymous"));
    }
    let Some(token) = header.unwrap_or("").strip_prefix("Bearer ") else {
        return Err(EhError::unauthorized(
            "Authorization: Bearer <token> is required",
        ));
    };
    authenticate_token(auth, token)
}

/// Resolve a bearer token to a caller with its per-namespace grants.
///
/// The one place grants are evaluated: the HTTP/MCP routes (either mode) and
/// in-process embedded calls all come through here, so a token means the same thing
/// however it reaches the engine.
pub fn authenticate_token(auth: &AuthConfig, token: &str) -> Result<Caller, EhError> {
    if auth.mode == "disabled" {
        return Ok(Caller::full("anonymous"));
    }
    if token.is_empty() {
        return Err(EhError::unauthorized("bearer token is empty"));
    }
    let grants: Vec<&Grant> = auth
        .grants
        .iter()
        .filter(|grant| grant.token == token)
        .collect();
    if grants.is_empty() {
        return Err(EhError::unauthorized("unknown credential"));
    }
    let principal = grants[0].principal.clone();
    if grants.iter().any(|grant| grant.principal != principal) {
        return Err(EhError::invalid("one token must not map to two principals"));
    }
    let mut read_all = false;
    let mut write_all = false;
    let mut read = BTreeSet::new();
    let mut write = BTreeSet::new();
    for grant in grants {
        if grant.namespace == "*" {
            read_all |= grant.read;
            write_all |= grant.write;
        } else {
            if grant.read {
                read.insert(grant.namespace.clone());
            }
            if grant.write {
                write.insert(grant.namespace.clone());
            }
        }
    }
    Ok(Caller {
        principal,
        read_all,
        write_all,
        read,
        write,
    })
}
