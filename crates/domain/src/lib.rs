//! Domain rules shared by the local server and the cloud worker.
//!
//! Validation mirrors the JavaScript API exactly (status, code and message), so it works
//! over `serde_json::Value`: an absent key is JavaScript `undefined`, `Value::Null` is `null`.

pub mod attachment;
pub mod cloud;
pub mod constants;
mod error;
pub mod fields;
pub mod input;
pub mod js;
pub mod records;
pub mod spec;
pub mod sql;

pub use error::{ApiError, Error, Result};
