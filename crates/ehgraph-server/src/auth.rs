use std::collections::BTreeSet;

use axum::http::HeaderMap;
use ehgraph_api::Caller;
use ehgraph_core::EhError;

use crate::config::{AuthConfig, Grant};

pub fn authenticate(auth: &AuthConfig, headers: &HeaderMap) -> Result<Caller, EhError> {
    if auth.mode == "disabled" {
        return Ok(Caller::full("anonymous"));
    }
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(token) = header.strip_prefix("Bearer ") else {
        return Err(EhError::unauthorized(
            "Authorization: Bearer <token> is required",
        ));
    };
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
