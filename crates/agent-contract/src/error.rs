use serde::{Deserialize, Serialize};

/// Structured failure. An unknown result uses `code = "unknown"` and is not a blank error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details_ref: Option<String>,
}

impl Error {
    #[must_use]
    pub fn unknown(message: impl Into<String>, details_ref: Option<String>) -> Self {
        Self {
            code: "unknown".to_owned(),
            message: message.into(),
            retryable: false,
            details_ref,
        }
    }
}
