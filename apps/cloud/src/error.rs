use broadcast_domain::ApiError;

/// An API error sent to the client, or an internal failure logged and reported as `500`.
pub enum Failure {
    Api(ApiError),
    Internal(String),
}

pub type Result<T, E = Failure> = std::result::Result<T, E>;

impl Failure {
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<broadcast_domain::Error> for Failure {
    fn from(error: broadcast_domain::Error) -> Self {
        match error {
            broadcast_domain::Error::Api(error) => Self::Api(error),
            broadcast_domain::Error::Internal(error) => Self::Internal(format!("{error:#}")),
        }
    }
}

impl From<worker::Error> for Failure {
    fn from(error: worker::Error) -> Self {
        Self::Internal(crate::db::message(&error))
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}
