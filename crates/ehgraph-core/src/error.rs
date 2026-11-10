use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error codes from the tool contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotFound,
    InvalidArgument,
    Unauthorized,
    SessionExpired,
    SessionLoading,
    MemoryBudgetExceeded,
    ReadOnlySession,
    Conflict,
    /// A conditional or transactional commit lost a race: an expected element or
    /// graph version no longer holds, or a concurrent commit changed what a
    /// transaction read. Nothing was written; re-read and retry.
    VersionConflict,
    Unavailable,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::SessionExpired => "SESSION_EXPIRED",
            Self::SessionLoading => "SESSION_LOADING",
            Self::MemoryBudgetExceeded => "MEMORY_BUDGET_EXCEEDED",
            Self::ReadOnlySession => "READ_ONLY_SESSION",
            Self::Conflict => "CONFLICT",
            Self::VersionConflict => "VERSION_CONFLICT",
            Self::Unavailable => "UNAVAILABLE",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidArgument => 400,
            Self::Unauthorized => 401,
            Self::NotFound => 404,
            Self::ReadOnlySession => 403,
            Self::Conflict | Self::VersionConflict | Self::SessionExpired => 409,
            Self::MemoryBudgetExceeded | Self::Unavailable | Self::SessionLoading => 503,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
pub struct EhError {
    pub code: ErrorCode,
    pub message: String,
}

impl EhError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, message)
    }

    pub fn to_body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code,
            message: self.message.clone(),
        }
    }
}

impl From<std::io::Error> for EhError {
    fn from(err: std::io::Error) -> Self {
        Self::unavailable(err.to_string())
    }
}

impl From<serde_json::Error> for EhError {
    fn from(err: serde_json::Error) -> Self {
        Self::unavailable(format!("json: {err}"))
    }
}
