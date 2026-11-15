//! D1 statements over JSON rows: a row is the object D1 returns, columns keep their SQL names.

use serde::Serialize;
use serde_json::{Map, Value};
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, D1Result};

use crate::error::{Failure, Result};

pub type Row = Map<String, Value>;

/// Rows fetched with `WHERE id IN (...)` are read in chunks below D1's bound-parameter limit.
pub const CHUNK: usize = 80;

/// The change revision the next write takes.
pub const NEXT_REVISION: &str = "(SELECT revision + 1 FROM global_revision WHERE singleton = 1)";

/// A bound parameter: JSON scalars map to their D1 types, arrays and objects to JSON text.
pub fn arg<T: Serialize + ?Sized>(value: &T) -> JsValue {
    match serde_json::to_value(value).unwrap_or(Value::Null) {
        Value::Null => JsValue::NULL,
        Value::Bool(value) => JsValue::from_bool(value),
        Value::Number(value) => JsValue::from_f64(value.as_f64().unwrap_or(f64::NAN)),
        Value::String(value) => JsValue::from_str(&value),
        value => JsValue::from_str(&value.to_string()),
    }
}

macro_rules! args {
    ($($value:expr),* $(,)?) => { vec![$($crate::db::arg(&$value)),*] };
}
pub(crate) use args;

pub fn statement(db: &D1Database, sql: impl Into<String>, args: Vec<JsValue>) -> Result<D1PreparedStatement> {
    let statement = db.prepare(sql);
    Ok(if args.is_empty() { statement } else { statement.bind(&args)? })
}

pub async fn first(db: &D1Database, sql: impl Into<String>, args: Vec<JsValue>) -> Result<Option<Row>> {
    Ok(statement(db, sql, args)?.first::<Row>(None).await?)
}

pub async fn all(db: &D1Database, sql: impl Into<String>, args: Vec<JsValue>) -> Result<Vec<Row>> {
    Ok(statement(db, sql, args)?.all().await?.results::<Row>()?)
}

pub async fn run(db: &D1Database, sql: impl Into<String>, args: Vec<JsValue>) -> Result<D1Result> {
    Ok(statement(db, sql, args)?.run().await?)
}

/// Whether a mutation changed a row.
pub fn changed(result: &D1Result) -> Result<bool> {
    let changes = result.meta()?.and_then(|meta| meta.changes);
    changes.map(|changes| changes > 0).ok_or_else(|| Failure::internal("D1 mutation did not return change metadata"))
}

/// The message of a D1 or JavaScript error, as JavaScript `error.message` reads it.
pub fn message(error: &worker::Error) -> String {
    match error {
        worker::Error::D1(error) => String::from(AsRef::<worker::js_sys::Error>::as_ref(error).message()),
        worker::Error::UnknownJsError { message, .. } => message.clone(),
        error => error.to_string(),
    }
}

/// `?, ?, ...` for `count` parameters.
pub fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

pub fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row.get(column).and_then(Value::as_str).unwrap_or_default()
}

pub fn optional_text<'a>(row: &'a Row, column: &str) -> Option<&'a str> {
    row.get(column).and_then(Value::as_str)
}

pub fn integer(row: &Row, column: &str) -> i64 {
    row.get(column).and_then(Value::as_f64).map_or(0, |value| value as i64)
}

pub fn value(row: &Row, column: &str) -> Value {
    row.get(column).cloned().unwrap_or(Value::Null)
}
