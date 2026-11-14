use std::borrow::Cow;

use serde_json::Value;

/// An error reported to API clients as `{ error: { code, message, details? } }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub code: Cow<'static, str>,
    pub message: String,
    pub details: Option<Box<Value>>,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into(), details: None }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }

    pub fn invalid_field(message: impl Into<String>) -> Self {
        Self::new(400, "INVALID_FIELD", message)
    }
}

/// An API error, or an internal failure reported as `500 INTERNAL_ERROR`.
#[derive(Debug)]
pub enum Error {
    Api(ApiError),
    Internal(anyhow::Error),
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}

impl From<Error> for anyhow::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Api(error) => anyhow::anyhow!("{} {}: {}", error.status, error.code, error.message),
            Error::Internal(error) => error,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
