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
        "decimal" | "money" => as_decimal(&v)?,
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
    let n = match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| Error::Invalid("not a number".into()))?,
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| Error::Invalid(format!("not a number: {s}")))?,
        _ => return Err(Error::Invalid("expected a number".into())),
    };
    // Reject non-finite values: JSON numbers can never be NaN/inf, but the
    // *string* forms parse ("NaN", "inf", "-infinity", "1e999" …) — and a
    // non-finite f64 converts to `Value::Null` (serde_json cannot represent
    // it), so the field would be silently stored as NULL instead of being
    // rejected… and a required field would sail past the required-check,
    // which only fires on the None branch.
    if !n.is_finite() {
        return Err(Error::Invalid(format!("not a finite number: {v}")));
    }
    Ok(n)
}

/// Decimal/money coercion with a JSON-model precision guard. The storage
/// contract is NUMERIC (money = NUMERIC(20,4), decimal(p,s) up to NUMERIC
/// domain), but a JSON number is an IEEE-754 binary64 with ~15.95 decimal
/// digits of significand: values with more significant digits are SILENTLY
/// rounded before they are ever stored (e.g. 9999999999999999.99 → 1e16).
/// Fail loudly instead — a value outside the exact-JSON domain is a client
/// error, not data to corrupt. (PLAN §5.1 hoisted-column rule; the bound is
/// the JSON numeric model, not the NUMERIC domain.)
fn as_decimal(v: &Value) -> Result<Value> {
    let text = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(Error::Invalid("expected a number".into())),
    };
    // Count significant decimal digits of the mantissa (sign, leading zeros,
    // decimal point, and exponent are not significant).
    let mantissa = text
        .split(['e', 'E'])
        .next()
        .unwrap_or(&text)
        .trim_start_matches(['+', '-']);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let significant = digits.trim_start_matches('0').len();
    if significant > 15 {
        return Err(Error::Invalid(format!(
            "decimal value has {significant} significant digits — beyond the 15 the JSON numeric model represents exactly; store it as a string field or reduce precision: {text}"
        )));
    }
    Ok(as_f64(v)?.into())
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
        _ => {
            return Err(Error::Invalid(
                "expected an ISO-8601 datetime string".into(),
            ))
        }
    };
    // Accept both RFC 3339 (compact) and ISO-8601 variants that chrono parses,
    // then NORMALIZE TO UTC (PLAN §5.1: "the DB never stores local time" — a
    // single canonical rendering also makes the DSL's string comparisons and
    // jsonb eq filters behave as instant equality instead of comparing
    // verbatim offsets).
    let dt = chrono::DateTime::parse_from_rfc3339(&s)
        .or_else(|_| {
            // Also try RFC 3339 without timezone offset (assume UTC).
            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f"))
                .map(|dt| dt.and_utc().into())
        })
        .map_err(|e| Error::Invalid(format!("invalid datetime '{s}': {e}")))?;
    Ok(Value::String(dt.with_timezone(&chrono::Utc).to_rfc3339()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_decimal_strings_are_rejected_not_null() {
        // "NaN"/"inf"/… parse as f64, but serde_json turns a non-finite f64
        // into Value::Null — pre-fix these were silently stored as NULL (and a
        // *required* decimal sailed past the required-check, which only fires
        // on the None branch).
        for bad in [
            "NaN",
            "nan",
            "inf",
            "-inf",
            "Infinity",
            "-infinity",
            "1e999",
        ] {
            let err = coerce("decimal", Some(Value::String(bad.into())))
                .expect_err("non-finite decimal must be rejected");
            assert!(
                err.to_string().contains("finite"),
                "unexpected error for {bad}: {err}"
            );
        }
        // …while finite values (number or numeric string) still coerce.
        assert_eq!(
            coerce("decimal", Some(Value::from(3.5))).unwrap(),
            Some(Value::from(3.5))
        );
        assert_eq!(
            coerce("decimal", Some(Value::String("3.5".into()))).unwrap(),
            Some(Value::from(3.5))
        );
        // money shares the as_f64 path.
        assert!(coerce("money", Some(Value::String("NaN".into()))).is_err());
    }
}
