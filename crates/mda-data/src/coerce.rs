//! Value coercion: validate and normalize an input JSON value for a field's
//! type so the stored `attributes` JSONB casts cleanly into any GENERATED column.

use mda_core::{Error, Result};
use serde_json::Value;

/// Coerce `value` to the JSON shape expected by `field_type`. `None` stays
/// `None` (required-ness is checked by the caller). Unknown / non-castable
/// values are rejected.
pub fn coerce(field_type: &str, value: Option<Value>) -> Result<Option<Value>> {
    let Some(v) = value else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    Ok(Some(match field_type {
        "string" | "text" | "enum" => as_string(&v)?,
        "integer" | "auto_number" => Value::from(as_i64(&v)?),
        "decimal" | "money" => as_f64(&v)?.into(),
        "bool" => Value::Bool(as_bool(&v)?),
        "date" | "datetime" => as_string(&v)?,
        "json" => v,
        other => return Err(Error::Invalid(format!("unsupported field type {other}"))),
    }))
}

fn as_string(v: &Value) -> Result<Value> {
    match v {
        Value::String(_) => Ok(v.clone()),
        Value::Number(n) => Ok(Value::String(n.to_string())),
        Value::Bool(b) => Ok(Value::String(b.to_string())),
        _ => Err(Error::Invalid("expected a string".into())),
    }
}

fn as_i64(v: &Value) -> Result<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| Error::Invalid("not an integer".into())),
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| Error::Invalid(format!("not an integer: {s}"))),
        _ => Err(Error::Invalid("expected an integer".into())),
    }
}

fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| Error::Invalid("not a number".into())),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| Error::Invalid(format!("not a number: {s}"))),
        _ => Err(Error::Invalid("expected a number".into())),
    }
}

fn as_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(Error::Invalid("expected a boolean".into())),
    }
}
