//! Value coercion: validate and normalize an input JSON value for a field's
//! type so the stored `attributes` JSONB casts cleanly into any GENERATED column.

use chrono::NaiveDate;
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
        "string" | "text" | "enum" | "attachment" => as_string(&v)?,
        "integer" | "auto_number" => Value::from(as_i64(&v)?),
        "decimal" | "money" => as_f64(&v)?.into(),
        "bool" => Value::Bool(as_bool(&v)?),
        "date" => as_date(&v)?,
        "datetime" => as_datetime(&v)?,
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

fn as_date(v: &Value) -> Result<Value> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(Error::Invalid("expected a date string (YYYY-MM-DD)".into())),
    };
    // Validate the date parses before storing it in JSONB.
    NaiveDate::parse_from_str(&s, "%Y-%m-%d")
        .map_err(|e| Error::Invalid(format!("invalid date '{s}': {e}")))?;
    Ok(Value::String(s))
}

fn as_datetime(v: &Value) -> Result<Value> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(Error::Invalid("expected an ISO-8601 datetime string".into())),
    };
    // Accept both RFC 3339 (compact) and ISO-8601 variants that chrono parses.
    let _ = chrono::DateTime::parse_from_rfc3339(&s)
        .or_else(|_| {
            // Also try RFC 3339 without timezone offset (assume UTC).
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f")
                })
                .map(|dt| dt.and_utc().into())
        })
        .map_err(|e| Error::Invalid(format!("invalid datetime '{s}': {e}")))?;
    Ok(Value::String(s))
}
