//! Sandboxed template engine (PLAN §5.19).
//!
//! A template body is text with `{{ … }}` interpolation markers. Each marker is:
//!  - a **dotted field path** (`{{ customer.name }}` → resolves the path against
//!    the render context), or
//!  - a **JSON DSL expression** (`{{ {"op":"Call","name":"upper","args":[…]} }}`)
//!    evaluated by the bounded expression engine (§5.2).
//!
//! Both paths reuse the bounded evaluator, so a template inherits its safety:
//! no arbitrary code, no I/O, cannot emit raw SQL, and bounded by depth/step
//! budgets (a pathological body cannot DoS the system). A render is additionally
//! capped by [`MAX_INTERPOLATIONS`].
//!
//! **AuthZ (§5.19):** the render context is AuthZ-filtered by the *caller* — a
//! template renders under the running user's field-level visibility, so it can
//! never emit a field the recipient cannot read. This module trusts the context
//! it is given; the API layer builds that context via the same FLS projection
//! the data API uses (§5.11).

use mda_core::{Error, Result};
use mda_expression::{eval, Expr, Registry};
use serde_json::Value;

/// Upper bound on interpolation markers in one body (a runaway template with
/// millions of `{{…}}` would otherwise be an amplification vector).
pub const MAX_INTERPOLATIONS: usize = 4_096;

/// A loaded template (from `meta.md_template`).
#[derive(Debug, Clone)]
pub struct Template {
    pub name: String,
    pub kind: String,
    pub body: String,
    pub content_type: String,
    pub locale: Option<String>,
}

/// Rendered output: the body plus the MIME type to serve/store it as.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub body: String,
    pub content_type: String,
}

/// Render `template` against `ctx` (a JSON object of variables: record fields,
/// actor, params). `reg` is the (pure) expression function registry.
pub fn render(template: &Template, ctx: &Value, reg: &Registry) -> Result<Rendered> {
    let out = render_body(&template.body, ctx, reg)?;
    Ok(Rendered {
        body: out,
        content_type: template.content_type.clone(),
    })
}

/// Render an arbitrary template body string against a context.
pub fn render_body(body: &str, ctx: &Value, reg: &Registry) -> Result<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    let mut count = 0usize;
    while i < chars.len() {
        if chars[i] == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            i += 2; // consume "{{"
            let start = i;
            let mut depth = 1u32;
            let mut closed = false;
            while i < chars.len() {
                let c = chars[i];
                if c == '{' {
                    depth = depth.saturating_add(1);
                } else if c == '}' {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        // need the next char to also be '}' to close the marker.
                        if i + 1 < chars.len() && chars[i + 1] == '}' {
                            closed = true;
                            break;
                        }
                    }
                }
                i += 1;
            }
            if !closed {
                return Err(Error::Invalid(
                    "template has an unclosed '{{' interpolation".into(),
                ));
            }
            let expr_src: String = chars[start..i].iter().collect();
            i += 2; // consume the closing "}}"
            count += 1;
            if count > MAX_INTERPOLATIONS {
                return Err(Error::Invalid(format!(
                    "template exceeds max interpolations ({MAX_INTERPOLATIONS})"
                )));
            }
            out.push_str(&render_interpolation(expr_src.trim(), ctx, reg)?);
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Resolve one interpolation marker to its string value.
fn render_interpolation(src: &str, ctx: &Value, reg: &Registry) -> Result<String> {
    if src.is_empty() {
        return Ok(String::new());
    }
    let value = if src.starts_with('{') {
        // JSON DSL expression.
        let v: Value = serde_json::from_str(src).map_err(Error::internal)?;
        let expr = Expr::from_json(&v)?;
        eval(&expr, ctx, reg)?
    } else {
        // Dotted field path (or a bare literal token). Resolve from context.
        resolve_path(ctx, src)?
    };
    Ok(value_to_string(&value))
}

/// Resolve a dotted path (`a.b.c`) against a JSON object. Missing segments →
/// Null (a template referencing an absent field renders empty, not an error —
/// the AuthZ-filtered context may legitimately omit a field).
fn resolve_path(ctx: &Value, path: &str) -> Result<Value> {
    let mut cur = ctx;
    for seg in path.split('.') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        cur = cur.get(seg).unwrap_or(&Value::Null);
    }
    Ok(cur.clone())
}

/// Render a JSON value as a template string. Strings are emitted verbatim;
/// booleans/numbers as their JSON literal; null → "" (an absent field renders
/// nothing); objects/arrays as compact JSON.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reg() -> Registry {
        Registry::new()
    }

    #[test]
    fn interpolates_fields_and_paths() {
        let ctx = json!({"name":"Acme","customer":{"tier":"Bronze"}});
        let t = Template {
            name: "x".into(),
            kind: "message".into(),
            body: "Hello {{ name }} ({{ customer.tier }})".into(),
            content_type: "text/plain".into(),
            locale: None,
        };
        let r = render(&t, &ctx, &reg()).unwrap();
        assert_eq!(r.body, "Hello Acme (Bronze)");
    }

    #[test]
    fn missing_field_renders_empty() {
        let ctx = json!({"name":"Acme"});
        let r = render_body("{{ missing }} {{ missing.deep }}", &ctx, &reg()).unwrap();
        assert_eq!(r, " ");
    }

    #[test]
    fn json_dsl_expression_evaluates() {
        let ctx = json!({"first":"Ada","last":"Lovelace"});
        let body = r#"{{ {"op":"Call","name":"concat","args":[{"op":"Field","name":"last"},{"op":"Lit","value":", "},{"op":"Field","name":"first"}]} }}"#;
        let r = render_body(body, &ctx, &reg()).unwrap();
        assert_eq!(r, "Lovelace, Ada");
    }

    #[test]
    fn bounded_interpolation_cap() {
        // A body with a single marker over the cap is rejected.
        let body = "{{ x }}".repeat(MAX_INTERPOLATIONS + 1);
        let err = render_body(&body, &json!({"x":1}), &reg()).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn unclosed_marker_errors() {
        let err = render_body("hello {{ name", &json!({}), &reg()).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn literal_text_passes_through() {
        let r = render_body("no markers here", &json!({}), &reg()).unwrap();
        assert_eq!(r, "no markers here");
    }
}
