use std::collections::{BTreeMap, BTreeSet};

use core_lib::{TaskEventDictionary, CONTAINER_CAPABILITY_ABI_VERSION};
use sha2::{Digest, Sha256};

use crate::ast::{BinaryOp, Expr, Statement};
use crate::oid_link::LinkedRelKind;
use crate::rel_symbol_discovery::LinkedRelSymbolSpec;
use crate::runtime_image::{RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage};
use crate::source_registry::SourceId;
use crate::SymbolId;

use super::{
    err, ContainerTaskBoundaryHint, ContainerTaskCapabilitySlot, ContainerTaskCodeRef,
    ContainerTaskDiscoveryError, ContainerTaskGraph, ContainerTaskKind, ContainerTaskSourceFile,
    ContainerTaskSourceSite,
};

const TASK_HASH_DOMAIN: &[u8] = b"RBE_CTI_TASK_PLAN_V1";

pub(super) fn symbol_hashes(
    image: &RuntimeImage,
    symbols: &BTreeSet<SymbolId>,
) -> Result<BTreeMap<SymbolId, String>, ContainerTaskDiscoveryError> {
    let mut out = BTreeMap::new();
    for symbol in symbols {
        out.insert(symbol.clone(), symbol_semantic_sha256(image, symbol)?);
    }
    Ok(out)
}

fn symbol_semantic_sha256(
    image: &RuntimeImage,
    symbol: &SymbolId,
) -> Result<String, ContainerTaskDiscoveryError> {
    let executable = image.executable(&symbol.source).ok_or_else(|| {
        err(format!(
            "missing executable for reachable Task symbol {symbol:?}"
        ))
    })?;
    let mut hash = Sha256::new();
    feed(&mut hash, b"RBE_CTI_SYMBOL_V1");
    feed(&mut hash, symbol.source.as_str().as_bytes());
    feed(&mut hash, symbol.name.as_bytes());

    match executable {
        RuntimeExecutable::Route(file) => {
            if let Some(verb) = symbol.name.strip_prefix("Route.") {
                let method = file
                    .methods
                    .iter()
                    .find(|method| method.verb == verb)
                    .ok_or_else(|| err(format!("missing Route method body for {}", symbol.name)))?;
                hash_optional_param(&mut hash, method.param_name.as_deref());
                hash_statements(&mut hash, &method.body);
            } else {
                let function = file
                    .functions
                    .iter()
                    .find(|function| function.name == symbol.name)
                    .ok_or_else(|| {
                        err(format!("missing Route function body for {}", symbol.name))
                    })?;
                hash_params(&mut hash, &function.params);
                hash_statements(&mut hash, &function.body);
            }
        }
        RuntimeExecutable::Module(file) => {
            let function = file
                .functions
                .iter()
                .find(|function| function.name == symbol.name)
                .ok_or_else(|| err(format!("missing Module function body for {}", symbol.name)))?;
            hash_params(&mut hash, &function.params);
            hash_statements(&mut hash, &function.body);
        }
        RuntimeExecutable::Service(file) => {
            if let Some(verb) = symbol.name.strip_prefix("Service.") {
                let method = file
                    .lifecycle
                    .iter()
                    .find(|method| method.verb == verb)
                    .ok_or_else(|| {
                        err(format!(
                            "missing Service lifecycle body for {}",
                            symbol.name
                        ))
                    })?;
                hash_optional_param(&mut hash, method.param_name.as_deref());
                hash_statements(&mut hash, &method.body);
            } else if let Some((class_name, method_name)) = symbol.name.split_once('.') {
                let class = file
                    .classes
                    .iter()
                    .find(|class| class.name == class_name)
                    .ok_or_else(|| err(format!("missing Service class for {}", symbol.name)))?;
                let method = class
                    .methods
                    .iter()
                    .find(|method| method.name == method_name)
                    .ok_or_else(|| {
                        err(format!("missing Service class method for {}", symbol.name))
                    })?;
                hash_params(&mut hash, &method.params);
                hash_statements(&mut hash, &method.body);
            } else {
                let function = file
                    .functions
                    .iter()
                    .find(|function| function.name == symbol.name)
                    .ok_or_else(|| {
                        err(format!("missing Service function body for {}", symbol.name))
                    })?;
                hash_params(&mut hash, &function.params);
                hash_statements(&mut hash, &function.body);
            }
        }
        RuntimeExecutable::Field(file) => {
            if symbol.name != "resolve" {
                return Err(err(format!("unknown Field Task symbol {}", symbol.name)));
            }
            let resolver = file
                .resolver
                .as_ref()
                .ok_or_else(|| err("reachable Field resolve symbol has no resolver body"))?;
            hash_params(&mut hash, &resolver.params);
            hash_statements(&mut hash, &resolver.body);
        }
        RuntimeExecutable::Server(file) => {
            let function = file
                .functions
                .iter()
                .find(|function| function.name == symbol.name)
                .ok_or_else(|| err(format!("missing Server function body for {}", symbol.name)))?;
            hash_params(&mut hash, &function.params);
            hash_statements(&mut hash, &function.body);
        }
    }

    Ok(hex::encode(hash.finalize()))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn task_semantic_hash(
    kind: ContainerTaskKind,
    id: &str,
    source: &SourceId,
    root: &SymbolId,
    graph: &ContainerTaskGraph,
    slots: &[ContainerTaskCapabilitySlot],
    hints: &[ContainerTaskBoundaryHint],
    required: &BTreeSet<String>,
    symbol_hashes: &BTreeMap<SymbolId, String>,
    code: &ContainerTaskCodeRef,
    logs: &TaskEventDictionary,
    files: &[ContainerTaskSourceFile],
    sites: &[ContainerTaskSourceSite],
    linked: &BTreeMap<String, &LinkedRelSymbolSpec>,
) -> String {
    let mut hash = Sha256::new();
    feed(&mut hash, TASK_HASH_DOMAIN);
    feed(&mut hash, &CONTAINER_CAPABILITY_ABI_VERSION.to_be_bytes());
    feed(
        &mut hash,
        &[match kind {
            ContainerTaskKind::RouteMethod => 1,
            ContainerTaskKind::ServiceExport => 2,
        }],
    );
    feed(&mut hash, id.as_bytes());
    feed(&mut hash, source.as_str().as_bytes());
    feed(&mut hash, root.name.as_bytes());

    for node in &graph.nodes {
        feed(&mut hash, &node.id.to_be_bytes());
        feed(&mut hash, &(node.kind as u16).to_be_bytes());
        for dependency in &node.dependencies {
            feed(&mut hash, &dependency.to_be_bytes());
        }
        for edge in [
            node.success_edge,
            node.failure_edge,
            node.timeout_edge,
            node.cancelled_edge,
        ] {
            feed(&mut hash, &edge.unwrap_or(u32::MAX).to_be_bytes());
        }
    }

    for slot in slots {
        feed(&mut hash, &slot.slot.to_be_bytes());
        feed(&mut hash, &(slot.node_kind as u16).to_be_bytes());
        hash_capability_requirement(&mut hash, &slot.requirement);
        for source in &slot.sources {
            feed(&mut hash, source.as_str().as_bytes());
        }
    }
    for hint in hints {
        feed(&mut hash, &hint.node_kind_code.to_be_bytes());
        feed(&mut hash, hint.source.as_str().as_bytes());
        feed(&mut hash, hint.label.as_bytes());
    }

    for (symbol, digest) in symbol_hashes {
        feed(&mut hash, symbol.source.as_str().as_bytes());
        feed(&mut hash, symbol.name.as_bytes());
        feed(&mut hash, digest.as_bytes());
    }

    // Linked-only dependencies contribute identity/kind/edge shape here. Their
    // exact sparse OID record hashes are added by the CTI assembler later; Phase
    // 2 must not over-invalidate by hashing an entire source file.
    for oid in required {
        feed(&mut hash, oid.as_bytes());
        if let Some(symbol) = linked.get(oid) {
            feed(&mut hash, &[linked_kind_code(symbol.kind)]);
            for dependency in &symbol.required_symbols {
                feed(&mut hash, dependency.as_bytes());
            }
        }
    }

    if let Some(value) = &code.artifact_sha256 {
        feed(&mut hash, value.as_bytes());
    }
    if let Some(value) = &code.interpreter_fallback {
        feed(&mut hash, value.as_bytes());
    }
    for event in logs.entries() {
        feed(&mut hash, &event.event_id.to_be_bytes());
        feed(&mut hash, &[event.class as u8, event.level as u8]);
        feed(&mut hash, event.symbol.as_bytes());
        feed(&mut hash, event.template.as_bytes());
    }
    for file in files {
        feed(&mut hash, &file.file_id.to_be_bytes());
        feed(&mut hash, file.path.as_bytes());
    }
    for site in sites {
        feed(&mut hash, &site.site_id.to_be_bytes());
        feed(&mut hash, site.source.as_str().as_bytes());
        feed(&mut hash, site.symbol.as_bytes());
        feed(&mut hash, &site.file_id.to_be_bytes());
        feed(&mut hash, &site.line.to_be_bytes());
        feed(&mut hash, &site.column.to_be_bytes());
    }
    hex::encode(hash.finalize())
}

fn hash_params(hash: &mut Sha256, params: &[String]) {
    feed(hash, &(params.len() as u64).to_be_bytes());
    for param in params {
        feed(hash, param.as_bytes());
    }
}

fn hash_optional_param(hash: &mut Sha256, param: Option<&str>) {
    match param {
        Some(param) => {
            feed(hash, &[1]);
            feed(hash, param.as_bytes());
        }
        None => feed(hash, &[0]),
    }
}

fn hash_statements(hash: &mut Sha256, statements: &[Statement]) {
    feed(hash, &(statements.len() as u64).to_be_bytes());
    for statement in statements {
        match statement {
            Statement::Const { name, value } => {
                feed(hash, &[1]);
                feed(hash, name.as_bytes());
                hash_expr(hash, value);
            }
            Statement::Return(value) => {
                feed(hash, &[2]);
                hash_expr(hash, value);
            }
            Statement::Expr(value) => {
                feed(hash, &[3]);
                hash_expr(hash, value);
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                feed(hash, &[4]);
                hash_expr(hash, condition);
                hash_statements(hash, then_body);
                hash_statements(hash, else_body);
            }
        }
    }
}

fn hash_expr(hash: &mut Sha256, expr: &Expr) {
    match expr {
        Expr::String(value) => {
            feed(hash, &[1]);
            feed(hash, value.as_bytes());
        }
        Expr::Number(value) => {
            feed(hash, &[2]);
            feed(hash, &value.to_bits().to_be_bytes());
        }
        Expr::Bool(value) => feed(hash, &[3, u8::from(*value)]),
        Expr::Null => feed(hash, &[4]),
        Expr::Ident(value) => {
            feed(hash, &[5]);
            feed(hash, value.as_bytes());
        }
        Expr::Member(target, member) => {
            feed(hash, &[6]);
            hash_expr(hash, target);
            feed(hash, member.as_bytes());
        }
        Expr::Call(callee, args) => {
            feed(hash, &[7]);
            hash_expr(hash, callee);
            feed(hash, &(args.len() as u64).to_be_bytes());
            for arg in args {
                hash_expr(hash, arg);
            }
        }
        Expr::Object(entries) => {
            feed(hash, &[8]);
            feed(hash, &(entries.len() as u64).to_be_bytes());
            for (key, value) in entries {
                feed(hash, key.as_bytes());
                hash_expr(hash, value);
            }
        }
        Expr::Array(values) => {
            feed(hash, &[9]);
            feed(hash, &(values.len() as u64).to_be_bytes());
            for value in values {
                hash_expr(hash, value);
            }
        }
        Expr::UnaryNot(value) => {
            feed(hash, &[10]);
            hash_expr(hash, value);
        }
        Expr::Binary { left, op, right } => {
            feed(hash, &[11, binary_op_code(*op)]);
            hash_expr(hash, left);
            hash_expr(hash, right);
        }
    }
}

fn binary_op_code(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Equal => 1,
        BinaryOp::StrictEqual => 2,
        BinaryOp::NotEqual => 3,
        BinaryOp::StrictNotEqual => 4,
        BinaryOp::Less => 5,
        BinaryOp::LessEqual => 6,
        BinaryOp::Greater => 7,
        BinaryOp::GreaterEqual => 8,
        BinaryOp::And => 9,
        BinaryOp::Or => 10,
        BinaryOp::Add => 11,
        BinaryOp::Subtract => 12,
        BinaryOp::Multiply => 13,
        BinaryOp::Divide => 14,
        BinaryOp::Modulo => 15,
    }
}

fn linked_kind_code(kind: LinkedRelKind) -> u8 {
    match kind {
        LinkedRelKind::Function => 1,
        LinkedRelKind::FirstClassFunction => 2,
        LinkedRelKind::ModuleExport => 3,
        LinkedRelKind::RouteExport => 4,
        LinkedRelKind::ServiceExport => 5,
        LinkedRelKind::Class => 6,
        LinkedRelKind::Constructor => 7,
        LinkedRelKind::Method => 8,
    }
}

fn hash_capability_requirement(hash: &mut Sha256, requirement: &RuntimeCapabilityRequirement) {
    match requirement {
        RuntimeCapabilityRequirement::PublicHttp { operation } => {
            feed(hash, &[1]);
            feed(hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Storage { owner, operation } => {
            feed(hash, &[2]);
            feed(hash, owner.as_bytes());
            feed(hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Video { owner, operation } => {
            feed(hash, &[3]);
            feed(hash, owner.as_bytes());
            feed(hash, operation.as_bytes());
        }
        RuntimeCapabilityRequirement::Service { service, operation } => {
            feed(hash, &[4]);
            feed(hash, service.as_bytes());
            feed(hash, operation.as_bytes());
        }
    }
}

fn feed(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
