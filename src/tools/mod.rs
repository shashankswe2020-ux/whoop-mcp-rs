//! Tool implementations (one module per tool family).

pub mod analytics;
pub mod baselines;
pub mod basic;
pub mod calendar;
pub mod compare;
pub mod dates;
pub mod sleep_debt;
pub mod stats;
pub mod today;
pub mod trend;
pub mod weekly;

use crate::api::ApiError;
use crate::api::pagination::PageError;
use crate::js::InvalidTime;

/// Failure of a tool handler, classified like the TypeScript error hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolError {
    /// WHOOP API, network, or auth failure.
    Api(ApiError),
    /// Invalid input or data (`ZodError` / `RangeError`).
    Invalid(String),
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(error) => error.fmt(f),
            Self::Invalid(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<PageError> for ToolError {
    fn from(error: PageError) -> Self {
        match error {
            PageError::Api(error) => Self::Api(error),
            PageError::Invalid => Self::Invalid(error.to_string()),
            PageError::Shape => Self::Other(error.to_string()),
        }
    }
}

impl From<InvalidTime> for ToolError {
    fn from(error: InvalidTime) -> Self {
        Self::Invalid(error.to_string())
    }
}

impl From<dates::InvalidDateExpression> for ToolError {
    fn from(error: dates::InvalidDateExpression) -> Self {
        Self::Other(error.0)
    }
}

/// Result type for tool handlers.
pub type ToolResult<T> = Result<T, ToolError>;
