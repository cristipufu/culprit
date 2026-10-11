use serde::Serialize;
use ts_rs::TS;

#[derive(Serialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    ConnectionFailed,
    AuthFailed,
    PermissionDenied,
    Timeout,
    Unsupported,
    QueryFailed,
    Internal,
}

#[derive(Serialize, TS, Clone, Debug, thiserror::Error)]
#[error("{message}")]
#[ts(export)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), hint: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Error::new(ErrorCode::InvalidInput, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(ErrorCode::NotFound, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(ErrorCode::Internal, message)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
