//! JavaScript semantics the API contract depends on.

use serde::Serializer;
use serde_json::{Map, Number, Value};

const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// `String.prototype.trim` whitespace: Unicode White_Space without U+0085, plus U+FEFF.
pub fn is_whitespace(c: char) -> bool {
    c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{85}')
}

pub fn trim(value: &str) -> &str {
    value.trim_matches(is_whitespace)
}

/// `String.prototype.length`: UTF-16 code units.
pub fn len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// The longest prefix with at most `units` UTF-16 code units.
pub fn prefix(value: &str, units: usize) -> &str {
    let mut count = 0;
    for (index, c) in value.char_indices() {
        count += c.len_utf16();
        if count > units {
            return &value[..index];
        }
    }
    value
}

/// `Number.isSafeInteger(value) ? value : undefined`.
pub fn safe_integer(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_number()?;
    if let Some(integer) = number.as_i64() {
        return (integer.abs() <= MAX_SAFE_INTEGER).then_some(integer);
    }
    if number.is_u64() {
        return None;
    }
    let float = number.as_f64()?;
    (float.is_finite() && float.trunc() == float && float.abs() <= MAX_SAFE_INTEGER as f64)
        .then_some(float as i64)
}

/// A JavaScript number as JSON: integral values print without a fraction.
pub fn number(value: f64) -> Value {
    if value.trunc() == value && value.abs() <= MAX_SAFE_INTEGER as f64 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

pub fn serialize_number<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.trunc() == *value && value.abs() <= MAX_SAFE_INTEGER as f64 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

/// `decodeURIComponent`, returning `None` where JavaScript throws `URIError`.
pub fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3).filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
            let text = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` as percent-encoded UTF-8.
pub fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `Object.keys` order: array-index keys ascending, then the rest in insertion order.
pub fn object_keys(object: &Map<String, Value>) -> Vec<&str> {
    let index = |key: &str| -> Option<u32> {
        let parsed: u32 = key.parse().ok()?;
        (parsed != u32::MAX && parsed.to_string() == key).then_some(parsed)
    };
    let mut indexed: Vec<(u32, &str)> =
        object.keys().filter_map(|key| index(key).map(|i| (i, key.as_str()))).collect();
    indexed.sort_unstable_by_key(|(i, _)| *i);
    indexed
        .into_iter()
        .map(|(_, key)| key)
        .chain(object.keys().map(String::as_str).filter(|key| index(key).is_none()))
        .collect()
}

/// JavaScript truthiness of a JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
