//! Conservative Phase-6 optimization for parsed REL functions.
//!
//! This pass deliberately runs before native lowering and only rewrites
//! semantics that RELC can prove locally. It is not a second evaluator and it
//! never executes calls, member access, host capabilities, or other potentially
//! effectful expressions during compilation.
//!
//! Initial Phase-6 scope:
//! - immutable scalar constant propagation;
//! - pure scalar constant folding;
//! - constant branch/CFG simplification;
//! - unreachable statement removal after guaranteed returns;
//! - dead pure `const` store removal;
//! - removal of pure literal expression statements.
//!
//! The optimizer is intentionally target-independent. Native lowering remains
//! responsible for target ABI/code generation, and unsupported optimized bodies
//! continue to fall back to the evaluator.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{BinaryOp, Expr, FunctionDef, Statement, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelOptimizationStats {
    pub constant_folds: usize,
    pub propagated_constants: usize,
    pub simplified_branches: usize,
    pub removed_dead_stores: usize,
    pub removed_unreachable_statements: usize,
    pub removed_pure_expr_statements: usize,
}

/// Optimize one function without mutating the parsed Runtime Image program.
///
/// Keeping this as a pure clone-in/clone-out transform makes optimizer adoption
/// transactional: evaluator-backed execution can continue using the original
/// AST while native lowering consumes the optimized form.
pub fn optimize_function(function: &FunctionDef) -> (FunctionDef, RelOptimizationStats) {
    let mut stats = RelOptimizationStats::default();
    let mut constants = BTreeMap::new();
    let body = optimize_block(&function.body, &mut constants, &mut stats);
    let (body, _) = remove_dead_stores(body, BTreeSet::new(), &mut stats);

    (
        FunctionDef {
            name: function.name.clone(),
            params: function.params.clone(),
            body,
        },
        stats,
    )
}

fn optimize_block(
    body: &[Statement],
    constants: &mut BTreeMap<String, Value>,
    stats: &mut RelOptimizationStats,
) -> Vec<Statement> {
    let mut out = Vec::with_capacity(body.len());

    for (index, statement) in body.iter().enumerate() {
        match statement {
            Statement::Const { name, value } => {
                let value = fold_expr(value, constants, stats);
                if let Some(value_constant) = scalar_value(&value) {
                    constants.insert(name.clone(), value_constant);
                } else {
                    constants.remove(name);
                }
                out.push(Statement::Const {
                    name: name.clone(),
                    value,
                });
            }
            Statement::Return(expr) => {
                out.push(Statement::Return(fold_expr(expr, constants, stats)));
                stats.removed_unreachable_statements = stats
                    .removed_unreachable_statements
                    .saturating_add(body.len().saturating_sub(index + 1));
                break;
            }
            Statement::Expr(expr) => {
                let expr = fold_expr(expr, constants, stats);
                if is_fully_literal(&expr) {
                    stats.removed_pure_expr_statements =
                        stats.removed_pure_expr_statements.saturating_add(1);
                } else {
                    out.push(Statement::Expr(expr));
                }
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let condition = fold_expr(condition, constants, stats);
                if let Some(condition_value) = scalar_value(&condition) {
                    stats.simplified_branches = stats.simplified_branches.saturating_add(1);
                    let chosen = if condition_value.truthy() {
                        then_body
                    } else {
                        else_body
                    };
                    let chosen = optimize_block(chosen, constants, stats);
                    let returns = block_always_returns(&chosen);
                    out.extend(chosen);
                    if returns {
                        stats.removed_unreachable_statements = stats
                            .removed_unreachable_statements
                            .saturating_add(body.len().saturating_sub(index + 1));
                        break;
                    }
                    continue;
                }

                let mut then_constants = constants.clone();
                let mut else_constants = constants.clone();
                let then_body = optimize_block(then_body, &mut then_constants, stats);
                let else_body = optimize_block(else_body, &mut else_constants, stats);

                // Both branches execute in the same function scope in REL. If
                // either branch writes a binding and the branch cannot be
                // chosen at compile time, the post-branch value is no longer a
                // provable scalar constant.
                let mut assigned = BTreeSet::new();
                collect_assigned_names(&then_body, &mut assigned);
                collect_assigned_names(&else_body, &mut assigned);
                for name in assigned {
                    constants.remove(&name);
                }

                let returns = !then_body.is_empty()
                    && !else_body.is_empty()
                    && block_always_returns(&then_body)
                    && block_always_returns(&else_body);
                out.push(Statement::If {
                    condition,
                    then_body,
                    else_body,
                });
                if returns {
                    stats.removed_unreachable_statements = stats
                        .removed_unreachable_statements
                        .saturating_add(body.len().saturating_sub(index + 1));
                    break;
                }
            }
        }
    }

    out
}

fn fold_expr(
    expr: &Expr,
    constants: &BTreeMap<String, Value>,
    stats: &mut RelOptimizationStats,
) -> Expr {
    match expr {
        Expr::String(value) => Expr::String(value.clone()),
        Expr::Number(value) => Expr::Number(*value),
        Expr::Bool(value) => Expr::Bool(*value),
        Expr::Null => Expr::Null,
        Expr::Ident(name) => {
            if let Some(value) = constants.get(name).and_then(value_to_expr) {
                stats.propagated_constants = stats.propagated_constants.saturating_add(1);
                value
            } else {
                Expr::Ident(name.clone())
            }
        }
        Expr::Member(base, field) => Expr::Member(
            Box::new(fold_expr(base, constants, stats)),
            field.clone(),
        ),
        Expr::Call(callee, args) => {
            // Never rewrite the callee identity: `foo()` may name a local or
            // imported callable even when a same-named value binding exists.
            Expr::Call(
                callee.clone(),
                args.iter()
                    .map(|arg| fold_expr(arg, constants, stats))
                    .collect(),
            )
        }
        Expr::Object(fields) => Expr::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), fold_expr(value, constants, stats)))
                .collect(),
        ),
        Expr::Array(items) => Expr::Array(
            items
                .iter()
                .map(|item| fold_expr(item, constants, stats))
                .collect(),
        ),
        Expr::UnaryNot(value) => {
            let value = fold_expr(value, constants, stats);
            if let Some(value) = scalar_value(&value) {
                stats.constant_folds = stats.constant_folds.saturating_add(1);
                Expr::Bool(!value.truthy())
            } else {
                Expr::UnaryNot(Box::new(value))
            }
        }
        Expr::Binary { left, op, right } => {
            let left = fold_expr(left, constants, stats);

            // Preserve the evaluator's short-circuit behavior. A decisive left
            // operand means the right expression must not even be inspected by
            // this pass because it may contain a call or a runtime error.
            if let Some(left_value) = scalar_value(&left) {
                match op {
                    BinaryOp::And if !left_value.truthy() => {
                        stats.constant_folds = stats.constant_folds.saturating_add(1);
                        return Expr::Bool(false);
                    }
                    BinaryOp::Or if left_value.truthy() => {
                        stats.constant_folds = stats.constant_folds.saturating_add(1);
                        return Expr::Bool(true);
                    }
                    _ => {}
                }
            }

            let right = fold_expr(right, constants, stats);
            if let (Some(left_value), Some(right_value)) =
                (scalar_value(&left), scalar_value(&right))
            {
                if let Some(value) = eval_scalar_binary(*op, left_value, right_value) {
                    stats.constant_folds = stats.constant_folds.saturating_add(1);
                    if let Some(expr) = value_to_expr(&value) {
                        return expr;
                    }
                }
            }

            Expr::Binary {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            }
        }
    }
}

fn scalar_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::String(value) => Some(Value::String(value.clone())),
        Expr::Number(value) => Some(Value::Number(*value)),
        Expr::Bool(value) => Some(Value::Bool(*value)),
        Expr::Null => Some(Value::Null),
        _ => None,
    }
}

fn value_to_expr(value: &Value) -> Option<Expr> {
    match value {
        Value::String(value) => Some(Expr::String(value.clone())),
        Value::Number(value) => Some(Expr::Number(*value)),
        Value::Bool(value) => Some(Expr::Bool(*value)),
        Value::Null => Some(Expr::Null),
        Value::Object(_) | Value::Array(_) => None,
    }
}

fn eval_scalar_binary(op: BinaryOp, left: Value, right: Value) -> Option<Value> {
    match op {
        BinaryOp::And => Some(Value::Bool(left.truthy() && right.truthy())),
        BinaryOp::Or => Some(Value::Bool(left.truthy() || right.truthy())),
        BinaryOp::Equal | BinaryOp::StrictEqual => Some(Value::Bool(value_eq(&left, &right))),
        BinaryOp::NotEqual | BinaryOp::StrictNotEqual => {
            Some(Value::Bool(!value_eq(&left, &right)))
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            match (&left, &right) {
                (Value::Number(a), Value::Number(b)) => Some(Value::Bool(match op {
                    BinaryOp::Less => a < b,
                    BinaryOp::LessEqual => a <= b,
                    BinaryOp::Greater => a > b,
                    BinaryOp::GreaterEqual => a >= b,
                    _ => unreachable!(),
                })),
                (Value::String(a), Value::String(b)) => {
                    let order = a.cmp(b);
                    Some(Value::Bool(match op {
                        BinaryOp::Less => order.is_lt(),
                        BinaryOp::LessEqual => order.is_le(),
                        BinaryOp::Greater => order.is_gt(),
                        BinaryOp::GreaterEqual => order.is_ge(),
                        _ => unreachable!(),
                    }))
                }
                _ => None,
            }
        }
        BinaryOp::Add => match (left, right) {
            (Value::Number(a), Value::Number(b)) => Some(Value::Number(a + b)),
            (Value::String(a), Value::String(b)) => Some(Value::String(a + &b)),
            // The evaluator intentionally stringifies mixed values via Debug.
            // Leave that representation-sensitive case for runtime execution.
            _ => None,
        },
        BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo => {
            let (a, b) = match (left, right) {
                (Value::Number(a), Value::Number(b)) => (a, b),
                _ => return None,
            };
            Some(Value::Number(match op {
                BinaryOp::Subtract => a - b,
                BinaryOp::Multiply => a * b,
                BinaryOp::Divide => a / b,
                BinaryOp::Modulo => a % b,
                _ => unreachable!(),
            }))
        }
    }
}

fn value_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

fn is_fully_literal(expr: &Expr) -> bool {
    match expr {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => true,
        Expr::Object(fields) => fields.iter().all(|(_, value)| is_fully_literal(value)),
        Expr::Array(items) => items.iter().all(is_fully_literal),
        Expr::Ident(_)
        | Expr::Member(_, _)
        | Expr::Call(_, _)
        | Expr::UnaryNot(_)
        | Expr::Binary { .. } => false,
    }
}

fn block_always_returns(body: &[Statement]) -> bool {
    for statement in body {
        match statement {
            Statement::Return(_) => return true,
            Statement::If {
                then_body,
                else_body,
                ..
            } if !then_body.is_empty()
                && !else_body.is_empty()
                && block_always_returns(then_body)
                && block_always_returns(else_body) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn collect_assigned_names(body: &[Statement], out: &mut BTreeSet<String>) {
    for statement in body {
        match statement {
            Statement::Const { name, .. } => {
                out.insert(name.clone());
            }
            Statement::If {
                then_body,
                else_body,
                ..
            } => {
                collect_assigned_names(then_body, out);
                collect_assigned_names(else_body, out);
            }
            Statement::Return(_) | Statement::Expr(_) => {}
        }
    }
}

fn remove_dead_stores(
    body: Vec<Statement>,
    mut live: BTreeSet<String>,
    stats: &mut RelOptimizationStats,
) -> (Vec<Statement>, BTreeSet<String>) {
    let mut reversed = Vec::with_capacity(body.len());

    for statement in body.into_iter().rev() {
        match statement {
            Statement::Const { name, value } => {
                if !live.contains(&name) && is_fully_literal(&value) {
                    stats.removed_dead_stores = stats.removed_dead_stores.saturating_add(1);
                    continue;
                }
                live.remove(&name);
                collect_expr_identifiers(&value, &mut live);
                reversed.push(Statement::Const { name, value });
            }
            Statement::Return(expr) => {
                live.clear();
                collect_expr_identifiers(&expr, &mut live);
                reversed.push(Statement::Return(expr));
            }
            Statement::Expr(expr) => {
                collect_expr_identifiers(&expr, &mut live);
                reversed.push(Statement::Expr(expr));
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let live_after = live.clone();
                let (then_body, then_live) =
                    remove_dead_stores(then_body, live_after.clone(), stats);
                let (else_body, else_live) = remove_dead_stores(else_body, live_after, stats);
                live.extend(then_live);
                live.extend(else_live);
                collect_expr_identifiers(&condition, &mut live);
                reversed.push(Statement::If {
                    condition,
                    then_body,
                    else_body,
                });
            }
        }
    }

    reversed.reverse();
    (reversed, live)
}

fn collect_expr_identifiers(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Ident(name) => {
            out.insert(name.clone());
        }
        Expr::Member(base, _) | Expr::UnaryNot(base) => collect_expr_identifiers(base, out),
        Expr::Call(callee, args) => {
            collect_expr_identifiers(callee, out);
            for arg in args {
                collect_expr_identifiers(arg, out);
            }
        }
        Expr::Object(fields) => {
            for (_, value) in fields {
                collect_expr_identifiers(value, out);
            }
        }
        Expr::Array(items) => {
            for item in items {
                collect_expr_identifiers(item, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_identifiers(left, out);
            collect_expr_identifiers(right, out);
        }
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(body: Vec<Statement>) -> FunctionDef {
        FunctionDef {
            name: "ready".into(),
            params: Vec::new(),
            body,
        }
    }

    fn call(name: &str) -> Expr {
        Expr::Call(Box::new(Expr::Ident(name.into())), Vec::new())
    }

    #[test]
    fn propagates_scalar_const_and_removes_dead_store() {
        let input = function(vec![
            Statement::Const {
                name: "flag".into(),
                value: Expr::Bool(true),
            },
            Statement::Return(Expr::Ident("flag".into())),
        ]);
        let (optimized, stats) = optimize_function(&input);
        assert_eq!(optimized.body.len(), 1);
        assert!(matches!(optimized.body[0], Statement::Return(Expr::Bool(true))));
        assert_eq!(stats.propagated_constants, 1);
        assert_eq!(stats.removed_dead_stores, 1);
    }

    #[test]
    fn folds_condition_and_collapses_constant_branch() {
        let input = function(vec![
            Statement::If {
                condition: Expr::Binary {
                    left: Box::new(Expr::Number(1.0)),
                    op: BinaryOp::Less,
                    right: Box::new(Expr::Number(2.0)),
                },
                then_body: vec![Statement::Return(Expr::Bool(true))],
                else_body: vec![Statement::Return(Expr::Bool(false))],
            },
            Statement::Expr(call("never_called")),
        ]);
        let (optimized, stats) = optimize_function(&input);
        assert_eq!(optimized.body.len(), 1);
        assert!(matches!(optimized.body[0], Statement::Return(Expr::Bool(true))));
        assert_eq!(stats.constant_folds, 1);
        assert_eq!(stats.simplified_branches, 1);
        assert_eq!(stats.removed_unreachable_statements, 1);
    }

    #[test]
    fn effectful_dead_store_is_not_removed() {
        let input = function(vec![
            Statement::Const {
                name: "unused".into(),
                value: call("side_effect"),
            },
            Statement::Return(Expr::Bool(true)),
        ]);
        let (optimized, stats) = optimize_function(&input);
        assert_eq!(optimized.body.len(), 2);
        assert!(matches!(optimized.body[0], Statement::Const { .. }));
        assert_eq!(stats.removed_dead_stores, 0);
    }

    #[test]
    fn unknown_branch_assignment_invalidates_propagation() {
        let input = function(vec![
            Statement::Const {
                name: "flag".into(),
                value: Expr::Bool(true),
            },
            Statement::If {
                condition: Expr::Ident("condition".into()),
                then_body: vec![Statement::Const {
                    name: "flag".into(),
                    value: Expr::Bool(false),
                }],
                else_body: Vec::new(),
            },
            Statement::Return(Expr::Ident("flag".into())),
        ]);
        let (optimized, _) = optimize_function(&input);
        assert!(matches!(
            optimized.body.last(),
            Some(Statement::Return(Expr::Ident(name))) if name == "flag"
        ));
    }

    #[test]
    fn decisive_short_circuit_never_folds_right_side() {
        let input = function(vec![Statement::Return(Expr::Binary {
            left: Box::new(Expr::Bool(false)),
            op: BinaryOp::And,
            right: Box::new(call("dangerous")),
        })]);
        let (optimized, stats) = optimize_function(&input);
        assert!(matches!(optimized.body[0], Statement::Return(Expr::Bool(false))));
        assert_eq!(stats.constant_folds, 1);
    }

    #[test]
    fn runtime_erroring_arithmetic_is_left_for_runtime() {
        let input = function(vec![Statement::Return(Expr::Binary {
            left: Box::new(Expr::String("nope".into())),
            op: BinaryOp::Subtract,
            right: Box::new(Expr::Number(1.0)),
        })]);
        let (optimized, stats) = optimize_function(&input);
        assert!(matches!(optimized.body[0], Statement::Return(Expr::Binary { .. })));
        assert_eq!(stats.constant_folds, 0);
    }
}
