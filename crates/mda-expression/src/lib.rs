//! `mda-expression` — the bounded JSON-AST expression DSL (PLAN §5.2).
//!
//! Used by rules (conditions + set-field values), field validations, calculated
//! fields, and workflow guards. The evaluator is **bounded** (max depth, max
//! node count, step budget) so a pathological or hostile expression cannot DoS
//! the system (REVIEW.md U6). Functions are pure; I/O-capable ones are
//! allowlisted via the [`Registry`].

use std::sync::Arc;

use mda_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum AST nesting depth and total step budget per evaluation.
pub const MAX_DEPTH: u32 = 32;
pub const STEP_BUDGET: u32 = 10_000;

/// A typed expression AST, serialized as JSON with an `op` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Expr {
    /// A literal JSON value.
    Lit {
        value: Value,
    },
    /// Reference to a record field by name.
    Field {
        name: String,
    },
    /// Logical connectives.
    And {
        of: Vec<Expr>,
    },
    Or {
        of: Vec<Expr>,
    },
    Not {
        of: Box<Expr>,
    },
    /// Comparison: kind ∈ eq | ne | lt | le | gt | ge.
    Cmp {
        kind: String,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Arithmetic: kind ∈ add | sub | mul | div.
    Arith {
        kind: String,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// A registered function call (pure; allowlisted).
    Call {
        name: String,
        args: Vec<Expr>,
    },
    /// Conditional.
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
}

impl Expr {
    /// Parse an expression from a JSON value.
    pub fn from_json(v: &Value) -> Result<Expr> {
        let expr: Expr = serde_json::from_value(v.clone()).map_err(Error::internal)?;
        Ok(expr)
    }
}

/// A registry of pure functions callable from the DSL.
#[derive(Clone, Default)]
pub struct Registry {
    #[allow(clippy::type_complexity)]
    fns: std::collections::HashMap<String, Arc<dyn Fn(&[Value]) -> Result<Value> + Send + Sync>>,
}

impl Registry {
    pub fn new() -> Self {
        let mut r = Self::default();
        r.add("now", |_| {
            Ok(Value::String(chrono::Utc::now().to_rfc3339()))
        });
        r.add("today", |_| {
            Ok(Value::String(chrono::Utc::now().date_naive().to_string()))
        });
        r.add("len", |a| {
            let s = as_str(a, 0)?;
            Ok(Value::from(s.chars().count() as i64))
        });
        r.add("upper", |a| Ok(Value::String(as_str(a, 0)?.to_uppercase())));
        r.add("lower", |a| Ok(Value::String(as_str(a, 0)?.to_lowercase())));
        r.add("coalesce", |a| {
            Ok(a.iter()
                .find(|v| !v.is_null())
                .cloned()
                .unwrap_or(Value::Null))
        });
        r.add("concat", |a| {
            let mut s = String::new();
            for v in a {
                if let Some(x) = v.as_str() {
                    s.push_str(x);
                } else {
                    s.push_str(&v.to_string());
                }
            }
            Ok(Value::String(s))
        });
        r
    }

    pub fn add<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        self.fns.insert(name.to_string(), Arc::new(f));
    }

    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let f = self
            .fns
            .get(name)
            .ok_or_else(|| Error::Invalid(format!("unknown function {name}")))?;
        f(args)
    }
}

/// Evaluate `expr` against a record context (`ctx` is a JSON object of field
/// values) using `reg`. Bounded by [`MAX_DEPTH`] / [`STEP_BUDGET`].
pub fn eval(expr: &Expr, ctx: &Value, reg: &Registry) -> Result<Value> {
    let mut steps = STEP_BUDGET;
    eval_inner(expr, ctx, reg, 0, &mut steps)
}

/// Truthiness of a value (used by rule conditions).
pub fn truth(v: &Value) -> bool {
    as_bool(v)
}

fn eval_inner(
    expr: &Expr,
    ctx: &Value,
    reg: &Registry,
    depth: u32,
    steps: &mut u32,
) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid(format!(
            "expression exceeds max depth {MAX_DEPTH}"
        )));
    }
    if *steps == 0 {
        return Err(Error::Invalid("expression step budget exhausted".into()));
    }
    *steps -= 1;
    Ok(match expr {
        Expr::Lit { value } => value.clone(),
        Expr::Field { name } => ctx.get(name).cloned().unwrap_or(Value::Null),
        Expr::And { of } => {
            for e in of {
                if !as_bool(&eval_inner(e, ctx, reg, depth + 1, steps)?) {
                    return Ok(Value::Bool(false));
                }
            }
            Value::Bool(true)
        }
        Expr::Or { of } => {
            for e in of {
                if as_bool(&eval_inner(e, ctx, reg, depth + 1, steps)?) {
                    return Ok(Value::Bool(true));
                }
            }
            Value::Bool(false)
        }
        Expr::Not { of } => Value::Bool(!as_bool(&eval_inner(of, ctx, reg, depth + 1, steps)?)),
        Expr::Cmp { kind, lhs, rhs } => {
            let l = eval_inner(lhs, ctx, reg, depth + 1, steps)?;
            let r = eval_inner(rhs, ctx, reg, depth + 1, steps)?;
            Value::Bool(compare(kind, &l, &r)?)
        }
        Expr::Arith { kind, lhs, rhs } => {
            let l = eval_inner(lhs, ctx, reg, depth + 1, steps)?;
            let r = eval_inner(rhs, ctx, reg, depth + 1, steps)?;
            arith(kind, &l, &r)?
        }
        Expr::If { cond, then, els } => {
            if as_bool(&eval_inner(cond, ctx, reg, depth + 1, steps)?) {
                eval_inner(then, ctx, reg, depth + 1, steps)?
            } else {
                eval_inner(els, ctx, reg, depth + 1, steps)?
            }
        }
        Expr::Call { name, args } => {
            let mut argv = Vec::with_capacity(args.len());
            for a in args {
                argv.push(eval_inner(a, ctx, reg, depth + 1, steps)?);
            }
            reg.call(name, &argv)?
        }
    })
}

fn as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn as_str(args: &[Value], i: usize) -> Result<&str> {
    args.get(i)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid(format!("argument {i} must be a string")))
}

fn compare(kind: &str, l: &Value, r: &Value) -> Result<bool> {
    use std::cmp::Ordering;
    let ord = match (l, r) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .map(|(x, y)| x.partial_cmp(&y).unwrap_or(Ordering::Equal))
            .unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => {
            return Err(Error::Invalid(
                "cannot compare values of these types".into(),
            ))
        }
    };
    Ok(match kind {
        "eq" => ord == Ordering::Equal,
        "ne" => ord != Ordering::Equal,
        "lt" => ord == Ordering::Less,
        "le" => ord != Ordering::Greater,
        "gt" => ord == Ordering::Greater,
        "ge" => ord != Ordering::Less,
        other => return Err(Error::Invalid(format!("unknown comparison {other}"))),
    })
}

fn arith(kind: &str, l: &Value, r: &Value) -> Result<Value> {
    let a = l
        .as_f64()
        .ok_or_else(|| Error::Invalid("arithmetic on non-number".into()))?;
    let b = r
        .as_f64()
        .ok_or_else(|| Error::Invalid("arithmetic on non-number".into()))?;
    let v = match kind {
        "add" => a + b,
        "sub" => a - b,
        "mul" => a * b,
        "div" => {
            if b == 0.0 {
                return Err(Error::Invalid("division by zero".into()));
            }
            a / b
        }
        other => return Err(Error::Invalid(format!("unknown arithmetic {other}"))),
    };
    Ok(serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({ "status": "Closed", "amount": 100, "qty": 2 })
    }

    fn ev(expr: &Value) -> Value {
        eval(&Expr::from_json(expr).unwrap(), &ctx(), &Registry::new()).unwrap()
    }

    #[test]
    fn literal_and_field() {
        assert_eq!(ev(&json!({"op":"Lit","value":"x"})), json!("x"));
        assert_eq!(ev(&json!({"op":"Field","name":"status"})), json!("Closed"));
        assert_eq!(ev(&json!({"op":"Field","name":"missing"})), json!(null));
    }

    #[test]
    fn comparison_and_logic() {
        let e = json!({"op":"Cmp","kind":"eq","lhs":{"op":"Field","name":"status"},"rhs":{"op":"Lit","value":"Closed"}});
        assert_eq!(ev(&e), json!(true));
        let e = json!({"op":"And","of":[
            {"op":"Cmp","kind":"gt","lhs":{"op":"Field","name":"amount"},"rhs":{"op":"Lit","value":50}},
            {"op":"Not","of":{"op":"Cmp","kind":"eq","lhs":{"op":"Field","name":"status"},"rhs":{"op":"Lit","value":"Open"}}}
        ]});
        assert_eq!(ev(&e), json!(true));
    }

    #[test]
    fn arithmetic() {
        let e = json!({"op":"Arith","kind":"mul","lhs":{"op":"Field","name":"amount"},"rhs":{"op":"Field","name":"qty"}});
        assert_eq!(ev(&e).as_f64().unwrap(), 200.0);
    }

    #[test]
    fn call_now_and_conditional() {
        let e = json!({"op":"Call","name":"now","args":[]});
        assert!(ev(&e).as_str().unwrap().contains('T'));
        let e = json!({"op":"If","cond":{"op":"Cmp","kind":"eq","lhs":{"op":"Field","name":"status"},"rhs":{"op":"Lit","value":"Closed"}},"then":{"op":"Lit","value":"done"},"els":{"op":"Lit","value":"open"}});
        assert_eq!(ev(&e), json!("done"));
    }

    #[test]
    fn depth_limit_rejects_deep_nesting() {
        // build a deeply nested Not chain
        let mut e = json!({"op":"Lit","value":true});
        for _ in 0..(MAX_DEPTH + 5) {
            e = json!({"op":"Not","of":e});
        }
        let expr = Expr::from_json(&e).unwrap();
        assert!(eval(&expr, &ctx(), &Registry::new()).is_err());
    }

    #[test]
    fn type_mismatch_errors() {
        let e = json!({"op":"Cmp","kind":"eq","lhs":{"op":"Lit","value":1},"rhs":{"op":"Lit","value":"x"}});
        assert!(eval(&Expr::from_json(&e).unwrap(), &ctx(), &Registry::new()).is_err());
    }
}
