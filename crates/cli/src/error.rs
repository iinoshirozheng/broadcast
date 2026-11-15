use serde_json::{Map, Value, json};

/// A failure reported as `{schemaVersion, error: {code, message, details?}}` on stderr.
pub struct CliError(Box<Fields>);

struct Fields {
    message: String,
    /// A board API error passes its own code through, whatever its JSON type.
    code: Value,
    exit_code: i32,
    details: Option<Value>,
}

pub type Result<T> = std::result::Result<T, CliError>;

impl CliError {
    pub fn new(message: impl Into<String>, code: &str, exit_code: i32) -> Self {
        Self::api(message.into(), code.into(), exit_code, None)
    }

    /// An error reported by the board API, with its code and details as sent.
    pub fn api(message: String, code: Value, exit_code: i32, details: Option<Value>) -> Self {
        Self(Box::new(Fields {
            message,
            code,
            exit_code,
            details,
        }))
    }

    pub fn details(mut self, details: impl ToString) -> Self {
        self.0.details = Some(Value::String(details.to_string()));
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.0.exit_code
    }

    pub fn to_json(&self) -> Value {
        let mut error = Map::new();
        error.insert("code".into(), self.0.code.clone());
        error.insert("message".into(), Value::String(self.0.message.clone()));
        if let Some(details) = &self.0.details {
            error.insert("details".into(), details.clone());
        }
        json!({ "schemaVersion": crate::SCHEMA_VERSION, "error": error })
    }
}

pub fn usage(message: impl Into<String>) -> CliError {
    CliError::new(message, "USAGE_ERROR", 2)
}

pub fn file_read(message: String, error: std::io::Error) -> CliError {
    CliError::new(message, "FILE_READ_FAILED", 2).details(error)
}
