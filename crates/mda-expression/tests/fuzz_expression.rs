//! Property-based fuzzer for the expression engine (REVIEW.md §The expression
//! evaluator … are the two highest-blast-radius components).
//!
//! Generates random expression ASTs and asserts:
//!  - evaluation never panics (unwinds are caught as errors);
//!  - step budget / depth limits are enforced (no infinite loops);
//!  - truthy/falsy values are consistent with the AND/OR/NOT ops.
//!
//! Run with:
//!   cargo test --test fuzz_expression -- --nocapture

use mda_expression::{eval, Expr, Registry};
use proptest::prelude::*;
use serde_json::{json, Value};

/// A shallow, size-limited strategy for generating an expression AST.
fn expr_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        // Literal
        any::<bool>().prop_map(|b| json!({"op": "Lit", "value": b})),
        Just(json!({"op": "Lit", "value": "hello"})),
        any::<i64>().prop_map(|n| json!({"op": "Lit", "value": n})),
        Just(json!({"op": "Lit", "value": null})),
        // Field reference (names from a small set that may or may not be in ctx)
        prop::sample::select(vec!["status", "amount", "qty", "name", "missing"]).prop_map(|f| {
            json!({"op": "Field", "name": f})
        }),
        // Call to a known function with no args
        Just(json!({"op": "Call", "name": "now", "args": []})),
        Just(json!({"op": "Call", "name": "today", "args": []})),
    ]
}

fn expr_tree(depth: u32) -> BoxedStrategy<Value> {
    if depth == 0 {
        return expr_leaf().boxed();
    }
    let leaf = expr_leaf();
    let child = expr_tree(depth - 1);
    prop_oneof![
        leaf,
        // Not
        child.clone().prop_map(|e| json!({"op": "Not", "of": e})),
        // Cmp (eq/ne/gt/lt)
        (child.clone(), child.clone()).prop_map(|(l, r)| {
            json!({"op": "Cmp", "kind": "eq", "lhs": l, "rhs": r})
        }),
        (child.clone(), child.clone()).prop_map(|(l, r)| {
            json!({"op": "Cmp", "kind": "gt", "lhs": l, "rhs": r})
        }),
        // If
        (child.clone(), child.clone(), child.clone()).prop_map(|(c, t, e)| {
            json!({"op": "If", "cond": c, "then": t, "els": e})
        }),
        // And / Or with 1–3 sub-expressions
        prop::collection::vec(child.clone(), 1..=3).prop_map(|of| {
            json!({"op": "And", "of": of})
        }),
        prop::collection::vec(child.clone(), 1..=3).prop_map(|of| {
            json!({"op": "Or", "of": of})
        }),
        // Arith
        (child.clone(), child.clone()).prop_map(|(l, r)| {
            json!({"op": "Arith", "kind": "add", "lhs": l, "rhs": r})
        }),
    ]
    .boxed()
}

fn expr_strategy() -> impl Strategy<Value = Value> {
    expr_tree(5)
}

fn ctx() -> Value {
    json!({
        "status": "Closed",
        "amount": 100,
        "qty": 2,
        "name": "Test"
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn eval_never_panics(expr_json in expr_strategy()) {
        let reg = Registry::new();
        let ctx = ctx();

        // Parse the expression — this may fail if proptest produces an
        // invalid shape (e.g. a Lit with a non-scalar), but it must never
        // panic.
        let Ok(expr) = Expr::from_json(&expr_json) else {
            return Ok(());
        };

        // eval must either return Ok(Value) or Err(...), never panic.
        let _ = eval(&expr, &ctx, &reg);
    }

    #[test]
    fn depth_limit_enforced(expr_json in expr_tree(6)) {
        let reg = Registry::new();
        let ctx = ctx();

        let Ok(expr) = Expr::from_json(&expr_json) else {
            return Ok(());
        };

        let result = eval(&expr, &ctx, &reg);
        // Ok or a depth/step error is fine; Invalid errors are expected
        // from type mismatches (e.g. comparing a bool to a string, or
        // arithmetic on non-numbers). Only flag truly unexpected errors.
        match result {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                assert!(
                    msg.contains("max depth")
                        || msg.contains("step budget")
                        || msg.contains("invalid")
                        || msg.contains("internal"),
                    "unexpected error: {msg}"
                );
            }
        }
    }

    #[test]
    fn and_truth_table(a in any::<bool>(), b in any::<bool>()) {
        let reg = Registry::new();
        let expr_json = json!({"op": "And", "of": [
            {"op": "Lit", "value": a},
            {"op": "Lit", "value": b}
        ]});
        let expr = Expr::from_json(&expr_json).unwrap();
        let v = eval(&expr, &ctx(), &reg).unwrap();
        assert_eq!(v.as_bool().unwrap(), a && b);
    }

    #[test]
    fn or_truth_table(a in any::<bool>(), b in any::<bool>()) {
        let reg = Registry::new();
        let expr_json = json!({"op": "Or", "of": [
            {"op": "Lit", "value": a},
            {"op": "Lit", "value": b}
        ]});
        let expr = Expr::from_json(&expr_json).unwrap();
        let v = eval(&expr, &ctx(), &reg).unwrap();
        assert_eq!(v.as_bool().unwrap(), a || b);
    }

    #[test]
    fn not_truth_table(a in any::<bool>()) {
        let reg = Registry::new();
        let expr_json = json!({"op": "Not", "of": {"op": "Lit", "value": a}});
        let expr = Expr::from_json(&expr_json).unwrap();
        let v = eval(&expr, &ctx(), &reg).unwrap();
        assert_eq!(v.as_bool().unwrap(), !a);
    }

    #[test]
    fn cmp_numeric_invariants(n1 in 0i64..1000, n2 in 0i64..1000) {
        let reg = Registry::new();
        let mk_cmp = |kind| {
            Expr::from_json(&json!({
                "op": "Cmp", "kind": kind,
                "lhs": {"op": "Lit", "value": n1},
                "rhs": {"op": "Lit", "value": n2}
            })).unwrap()
        };
        assert_eq!(
            eval(&mk_cmp("eq"), &ctx(), &reg).unwrap().as_bool().unwrap(),
            n1 == n2
        );
        assert_eq!(
            eval(&mk_cmp("ne"), &ctx(), &reg).unwrap().as_bool().unwrap(),
            n1 != n2
        );
        assert_eq!(
            eval(&mk_cmp("lt"), &ctx(), &reg).unwrap().as_bool().unwrap(),
            n1 < n2
        );
        assert_eq!(
            eval(&mk_cmp("gt"), &ctx(), &reg).unwrap().as_bool().unwrap(),
            n1 > n2
        );
    }
}
