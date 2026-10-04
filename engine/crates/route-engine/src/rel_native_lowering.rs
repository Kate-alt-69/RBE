//! Phase 2 native lowering for the first executable linked-REL leaf subset.
//!
//! This module is intentionally strict. A native fragment is emitted only when
//! RELC can preserve the complete observable semantics with the current OID
//! native ABI. Everything else stays an explicit evaluator fallback; no
//! placeholder `ret` fragments are ever manufactured.
//!
//! The initial executable subset is:
//! - callable linked REL function/export identities;
//! - zero parameters;
//! - no linked-symbol dependencies;
//! - no host capabilities;
//! - a body that Phase-6 scalar optimization proves equivalent to exactly one
//!   `return true;` or `return false;` statement.
//!
//! OID native ABI v1 returns that Boolean as canonical integer 0/1 in the
//! platform integer result register. `OID_FLAG_RETURNS_BOOL` makes the result
//! shape explicit for the future Service worker native-call bridge.

use crate::ast::{Expr, FunctionDef, Statement};
use crate::oid_link::{LinkedRelKind, LinkedRelSymbolSpec};
use crate::oid_materialize::NativeOidFragment;
use crate::rel_optimizer::optimize_function;
use crate::service_oid::{
    lower_native_bool_return, OidError, OidTarget, OID_FLAG_BASELINE_CPU, OID_FLAG_CALLABLE_LEAF,
    OID_FLAG_RETURNS_BOOL,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeRelLowering {
    Native(NativeOidFragment),
    EvaluatorFallback { reason: String },
}

impl NativeRelLowering {
    pub fn fallback_reason(&self) -> Option<&str> {
        match self {
            Self::Native(_) => None,
            Self::EvaluatorFallback { reason } => Some(reason),
        }
    }
}

/// Lower one already-linked REL callable into a target-native OID fragment.
///
/// The caller supplies the exact `LinkedRelSymbolSpec` chosen by RELC discovery
/// and its parsed function body. This function never allocates an OID and never
/// writes cache state; Phase 3/4 keeps ownership/publication transactional.
pub fn lower_linked_rel_function(
    symbol: &LinkedRelSymbolSpec,
    function: &FunctionDef,
    target: &OidTarget,
) -> Result<NativeRelLowering, OidError> {
    if !matches!(
        symbol.kind,
        LinkedRelKind::Function
            | LinkedRelKind::FirstClassFunction
            | LinkedRelKind::ModuleExport
            | LinkedRelKind::ServiceExport
    ) {
        return Ok(fallback(format!(
            "linked REL kind {:?} is not in the Phase-2 executable leaf subset",
            symbol.kind
        )));
    }
    if !symbol.required_symbols.is_empty() {
        return Ok(fallback(
            "linked REL callable has OID dependencies; call relocation lowering is not implemented yet",
        ));
    }
    if !symbol.capabilities.is_empty() {
        return Ok(fallback(
            "linked REL callable crosses a host capability; native capability stubs are not implemented yet",
        ));
    }
    if !function.params.is_empty() {
        return Ok(fallback(
            "linked REL callable has parameters; REL value argument marshalling is not implemented yet",
        ));
    }

    // Phase 6 deliberately optimizes a clone. The parsed Runtime Image function
    // remains untouched for evaluator fallback and diagnostics, while native
    // lowering gets the smallest provably-equivalent body we currently know how
    // to emit.
    let (optimized, _) = optimize_function(function);
    let value = match optimized.body.as_slice() {
        [Statement::Return(Expr::Bool(value))] => *value,
        _ => {
            return Ok(fallback(
                "callable remains outside the constant-Boolean leaf subset after Phase-6 scalar optimization",
            ))
        }
    };

    let mut fragment = NativeOidFragment::executable(lower_native_bool_return(value, target)?);
    fragment.flags = OID_FLAG_CALLABLE_LEAF | OID_FLAG_BASELINE_CPU | OID_FLAG_RETURNS_BOOL;
    fragment.alignment = match target.arch.as_str() {
        "aarch64" => 4,
        "x86_64" => 16,
        _ => unreachable!("native Boolean lowering validates the target before alignment"),
    };
    Ok(NativeRelLowering::Native(fragment))
}

fn fallback(reason: impl Into<String>) -> NativeRelLowering {
    NativeRelLowering::EvaluatorFallback {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::ast::BinaryOp;
    use crate::oid_materialize::materialize_rel_records;
    use crate::rel_oid_bridge::reconcile_rel_cache;
    use crate::service_oid::{OidCache, OidRecordKind};

    fn symbol(kind: LinkedRelKind) -> LinkedRelSymbolSpec {
        LinkedRelSymbolSpec {
            canonical_id: "service_demo_ready".into(),
            kind,
            source_sha256: "a".repeat(64),
            required_symbols: BTreeSet::new(),
            capabilities: BTreeSet::new(),
        }
    }

    fn bool_function(value: bool) -> FunctionDef {
        FunctionDef {
            name: "ready".into(),
            params: Vec::new(),
            body: vec![Statement::Return(Expr::Bool(value))],
        }
    }

    fn project_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rbe-rel-native-lowering-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn constant_boolean_leaf_emits_real_machine_code() {
        let target = OidTarget::current();
        let symbol = symbol(LinkedRelKind::ServiceExport);
        let true_lowering =
            lower_linked_rel_function(&symbol, &bool_function(true), &target).unwrap();
        let false_lowering =
            lower_linked_rel_function(&symbol, &bool_function(false), &target).unwrap();

        let NativeRelLowering::Native(true_fragment) = true_lowering else {
            panic!("constant true leaf should lower natively");
        };
        let NativeRelLowering::Native(false_fragment) = false_lowering else {
            panic!("constant false leaf should lower natively");
        };
        assert!(!true_fragment.machine_code.is_empty());
        assert!(!false_fragment.machine_code.is_empty());
        assert_ne!(true_fragment.machine_code, false_fragment.machine_code);
        assert_eq!(
            true_fragment.flags,
            OID_FLAG_CALLABLE_LEAF | OID_FLAG_BASELINE_CPU | OID_FLAG_RETURNS_BOOL
        );
        assert!(true_fragment.required_oids.is_empty());
        assert!(true_fragment.relocations.is_empty());
    }

    #[test]
    fn scalar_optimizer_expands_native_leaf_subset_without_widening_abi() {
        let target = OidTarget::current();
        let function = FunctionDef {
            name: "ready".into(),
            params: Vec::new(),
            body: vec![
                Statement::Const {
                    name: "enabled".into(),
                    value: Expr::Bool(true),
                },
                Statement::If {
                    condition: Expr::Binary {
                        left: Box::new(Expr::Number(2.0)),
                        op: BinaryOp::Greater,
                        right: Box::new(Expr::Number(1.0)),
                    },
                    then_body: vec![Statement::Return(Expr::Ident("enabled".into()))],
                    else_body: vec![Statement::Return(Expr::Bool(false))],
                },
            ],
        };

        let lowering =
            lower_linked_rel_function(&symbol(LinkedRelKind::ServiceExport), &function, &target)
                .unwrap();
        let NativeRelLowering::Native(fragment) = lowering else {
            panic!("optimizer-proven Boolean leaf should lower natively");
        };
        assert!(!fragment.machine_code.is_empty());
        assert_eq!(
            fragment.flags,
            OID_FLAG_CALLABLE_LEAF | OID_FLAG_BASELINE_CPU | OID_FLAG_RETURNS_BOOL
        );
        assert!(fragment.required_oids.is_empty());
        assert!(fragment.relocations.is_empty());
    }

    #[test]
    fn parameters_dependencies_and_capabilities_fallback_explicitly() {
        let target = OidTarget::current();

        let mut with_param = bool_function(true);
        with_param.params.push("value".into());
        let lowered =
            lower_linked_rel_function(&symbol(LinkedRelKind::ServiceExport), &with_param, &target)
                .unwrap();
        assert!(lowered
            .fallback_reason()
            .expect("parameterized function must fallback")
            .contains("marshalling"));

        let mut with_dependency = symbol(LinkedRelKind::ServiceExport);
        with_dependency
            .required_symbols
            .insert("module_math_ready".into());
        let lowered =
            lower_linked_rel_function(&with_dependency, &bool_function(true), &target).unwrap();
        assert!(lowered
            .fallback_reason()
            .expect("dependent function must fallback")
            .contains("relocation"));

        let mut with_capability = symbol(LinkedRelKind::ServiceExport);
        with_capability.capabilities.insert("network:public".into());
        let lowered =
            lower_linked_rel_function(&with_capability, &bool_function(true), &target).unwrap();
        assert!(lowered
            .fallback_reason()
            .expect("capability function must fallback")
            .contains("capability"));
    }

    #[test]
    fn unsupported_body_and_non_leaf_kind_do_not_get_placeholder_code() {
        let target = OidTarget::current();
        let function = FunctionDef {
            name: "ready".into(),
            params: Vec::new(),
            body: vec![Statement::Return(Expr::Null)],
        };
        let lowered =
            lower_linked_rel_function(&symbol(LinkedRelKind::ServiceExport), &function, &target)
                .unwrap();
        assert!(lowered.fallback_reason().is_some());

        let lowered = lower_linked_rel_function(
            &symbol(LinkedRelKind::Method),
            &bool_function(true),
            &target,
        )
        .unwrap();
        assert!(lowered.fallback_reason().is_some());
    }

    #[test]
    fn lowered_boolean_fragment_materializes_as_sparse_service_export_record() {
        let root = project_root("materialize");
        let mut cache = OidCache::open_or_rebuild(&root).expect("open OID cache");
        let symbol = symbol(LinkedRelKind::ServiceExport);
        let report =
            reconcile_rel_cache(&mut cache, std::slice::from_ref(&symbol), &BTreeSet::new())
                .expect("allocate linked REL OID");
        let oid = report.bindings[&symbol.canonical_id].oid;

        let lowering =
            lower_linked_rel_function(&symbol, &bool_function(true), &cache.index().target)
                .expect("lower Boolean leaf");
        let NativeRelLowering::Native(fragment) = lowering else {
            panic!("Boolean leaf must be native");
        };
        let materialized = materialize_rel_records(
            &cache,
            &report.bindings,
            &BTreeMap::from([(symbol.canonical_id.clone(), fragment)]),
        )
        .expect("materialize linked REL record");
        assert!(materialized.changed_oids.contains(&oid));

        let record = cache
            .read_record(oid)
            .expect("read materialized OID record");
        assert_eq!(record.kind, OidRecordKind::ServiceExport);
        assert!(record.flags & OID_FLAG_RETURNS_BOOL != 0);
        assert!(!record.machine_code.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }
}
