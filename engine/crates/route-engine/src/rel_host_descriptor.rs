//! Deferred REL host-operation descriptors.
//!
//! `script.run(...)` and `archive.*(...)` validate their arguments here and
//! return opaque REL objects. `workspace.temp(...)` can then materialize the
//! contained operation through the trusted `RelHostExecutor` after a temp root
//! exists, instead of executing the inner operation too early.

use std::collections::HashMap;

use crate::archive::ArchivePlan;
use crate::ast::Value;
use crate::module_eval::ModuleEvalError;
use crate::rel_host_runtime::RelHostRequest;
use crate::script::ScriptPlan;
use crate::workspace::{WorkspaceOperation, WorkspacePath, WorkspacePlan};

const MARKER: &str = "__rbeHostOperation";
const KIND: &str = "kind";
const PATH: &str = "path";
const ARGS: &str = "args";
const TIMEOUT_MS: &str = "timeoutMs";
const OPERATION: &str = "operation";
const ENTRY: &str = "entry";
const ENTRIES: &str = "entries";
const DESTINATION: &str = "destination";
const SOURCE: &str = "source";

pub(crate) fn deferred_call(
    module: &str,
    function: &str,
    args: Vec<Value>,
) -> Result<Option<Value>, ModuleEvalError> {
    match module {
        "script" => script_descriptor(function, args).map(Some),
        "archive" => archive_descriptor(function, args).map(Some),
        "workspace" if function == "construct" => workspace_construct(args).map(Some),
        "workspace" if function == "fetch" => workspace_fetch(args).map(Some),
        "workspace" if function == "copy" => workspace_copy(args).map(Some),
        "workspace" => Ok(None),
        _ => Ok(None),
    }
}

pub(crate) fn temp_request(args: &[Value]) -> Result<RelHostRequest, ModuleEvalError> {
    if args.len() != 1 {
        return Err(error("workspace.temp() expects exactly one deferred host operation"));
    }
    let inner = request_from_descriptor(&args[0])?;
    Ok(RelHostRequest::Temp(Box::new(inner)))
}

fn script_descriptor(function: &str, args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    if args.is_empty() || args.len() > 2 {
        return Err(error(format!(
            "script.{function}() expects path[, options]"
        )));
    }
    let path = string_arg(args.first(), "script path")?;
    let mut plan = match function {
        "run" => ScriptPlan::run(path),
        "runPyPy" | "run_pypy" => ScriptPlan::run_pypy(path),
        "runRust" | "run_rust" => ScriptPlan::run_rust(path),
        other => return Err(error(format!("script.{other}() does not exist"))),
    }
    .map_err(|e| error(e.to_string()))?;

    if let Some(options) = args.get(1) {
        let Value::Object(options) = options else {
            return Err(error("script options must be an object"));
        };
        if let Some(value) = options.get(ARGS) {
            let Value::Array(values) = value else {
                return Err(error("script options.args must be an array of strings"));
            };
            let mut parsed = Vec::with_capacity(values.len());
            for value in values {
                let Value::String(value) = value else {
                    return Err(error("script options.args must contain only strings"));
                };
                parsed.push(value.clone());
            }
            plan = plan.with_args(parsed).map_err(|e| error(e.to_string()))?;
        }
        if let Some(value) = options.get(TIMEOUT_MS) {
            let timeout = positive_u64(value, "script options.timeoutMs")?;
            plan = plan
                .with_timeout_ms(timeout)
                .map_err(|e| error(e.to_string()))?;
        }
    }

    Ok(script_plan_value(plan))
}

fn script_plan_value(plan: ScriptPlan) -> Value {
    Value::Object(HashMap::from([
        (MARKER.into(), Value::String("script".into())),
        (
            PATH.into(),
            Value::String(plan.path.workspace().symbolic()),
        ),
        (
            "runtime".into(),
            Value::String(plan.path.runtime_identity().into()),
        ),
        (
            ARGS.into(),
            Value::Array(plan.args.into_iter().map(Value::String).collect()),
        ),
        (TIMEOUT_MS.into(), Value::Number(plan.timeout_ms as f64)),
    ]))
}

fn archive_descriptor(function: &str, args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    let plan = match function {
        "list" => {
            expect_arity(function, &args, 1)?;
            ArchivePlan::list(string_arg(args.first(), "archive path")?)
        }
        "read" => {
            expect_arity(function, &args, 2)?;
            ArchivePlan::read(
                string_arg(args.first(), "archive path")?,
                string_arg(args.get(1), "archive entry")?,
            )
        }
        "extract" => {
            expect_arity(function, &args, 3)?;
            let Value::Array(entries) = &args[1] else {
                return Err(error("archive.extract() entries must be an array of strings"));
            };
            let mut parsed = Vec::with_capacity(entries.len());
            for entry in entries {
                let Value::String(entry) = entry else {
                    return Err(error("archive.extract() entries must contain only strings"));
                };
                parsed.push(entry.clone());
            }
            ArchivePlan::extract(
                string_arg(args.first(), "archive path")?,
                parsed,
                string_arg(args.get(2), "archive destination")?,
            )
        }
        "create" => {
            expect_arity(function, &args, 2)?;
            ArchivePlan::create(
                string_arg(args.first(), "archive source")?,
                string_arg(args.get(1), "archive destination")?,
            )
        }
        "replace" => {
            expect_arity(function, &args, 3)?;
            ArchivePlan::replace(
                string_arg(args.first(), "archive path")?,
                string_arg(args.get(1), "archive entry")?,
                string_arg(args.get(2), "replacement source")?,
            )
        }
        "remove" => {
            expect_arity(function, &args, 2)?;
            ArchivePlan::remove(
                string_arg(args.first(), "archive path")?,
                string_arg(args.get(1), "archive entry")?,
            )
        }
        other => return Err(error(format!("archive.{other}() does not exist"))),
    }
    .map_err(|e| error(e.to_string()))?;
    Ok(archive_plan_value(plan))
}

fn archive_plan_value(plan: ArchivePlan) -> Value {
    let mut fields = HashMap::from([(MARKER.into(), Value::String("archive".into()))]);
    match plan {
        ArchivePlan::List { archive, .. } => {
            fields.insert(OPERATION.into(), Value::String("list".into()));
            fields.insert(PATH.into(), Value::String(archive.symbolic()));
        }
        ArchivePlan::Read { archive, entry, .. } => {
            fields.insert(OPERATION.into(), Value::String("read".into()));
            fields.insert(PATH.into(), Value::String(archive.symbolic()));
            fields.insert(ENTRY.into(), Value::String(entry.as_str().into()));
        }
        ArchivePlan::Extract {
            archive,
            entries,
            destination,
            ..
        } => {
            fields.insert(OPERATION.into(), Value::String("extract".into()));
            fields.insert(PATH.into(), Value::String(archive.symbolic()));
            fields.insert(
                ENTRIES.into(),
                Value::Array(
                    entries
                        .into_iter()
                        .map(|entry| Value::String(entry.as_str().into()))
                        .collect(),
                ),
            );
            fields.insert(DESTINATION.into(), Value::String(destination.symbolic()));
        }
        ArchivePlan::Create {
            source,
            destination,
            ..
        } => {
            fields.insert(OPERATION.into(), Value::String("create".into()));
            fields.insert(SOURCE.into(), Value::String(source.symbolic()));
            fields.insert(DESTINATION.into(), Value::String(destination.symbolic()));
        }
        ArchivePlan::Replace {
            archive,
            entry,
            source,
            ..
        } => {
            fields.insert(OPERATION.into(), Value::String("replace".into()));
            fields.insert(PATH.into(), Value::String(archive.symbolic()));
            fields.insert(ENTRY.into(), Value::String(entry.as_str().into()));
            fields.insert(SOURCE.into(), Value::String(source.symbolic()));
        }
        ArchivePlan::Remove { archive, entry, .. } => {
            fields.insert(OPERATION.into(), Value::String("remove".into()));
            fields.insert(PATH.into(), Value::String(archive.symbolic()));
            fields.insert(ENTRY.into(), Value::String(entry.as_str().into()));
        }
    }
    Value::Object(fields)
}

fn workspace_construct(args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    expect_arity("workspace.construct", &args, 0)?;
    Ok(Value::Object(HashMap::from([
        (MARKER.into(), Value::String("workspace-plan".into())),
        ("steps".into(), Value::Array(Vec::new())),
    ])))
}

fn workspace_fetch(args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    expect_arity("workspace.fetch", &args, 1)?;
    let Value::Object(options) = &args[0] else {
        return Err(error("workspace.fetch() expects an options object"));
    };
    let source = string_arg(options.get(SOURCE), "workspace fetch source")?;
    if !source.starts_with("https://") {
        return Err(error("workspace.fetch() source must use HTTPS"));
    }
    let destination = WorkspacePath::parse(string_arg(
        options.get("to"),
        "workspace fetch destination",
    )?)
    .map_err(|e| error(e.to_string()))?;
    Ok(workspace_operation_value(WorkspaceOperation::Fetch {
        source: source.to_string(),
        destination,
    }))
}

fn workspace_copy(args: Vec<Value>) -> Result<Value, ModuleEvalError> {
    expect_arity("workspace.copy", &args, 2)?;
    let source = WorkspacePath::parse(string_arg(args.first(), "workspace copy source")?)
        .map_err(|e| error(e.to_string()))?;
    let destination = WorkspacePath::parse(string_arg(
        args.get(1),
        "workspace copy destination",
    )?)
    .map_err(|e| error(e.to_string()))?;
    Ok(workspace_operation_value(WorkspaceOperation::Copy {
        source,
        destination,
    }))
}

fn workspace_operation_value(operation: WorkspaceOperation) -> Value {
    let mut fields = HashMap::from([(MARKER.into(), Value::String("workspace-op".into()))]);
    match operation {
        WorkspaceOperation::Fetch {
            source,
            destination,
        } => {
            fields.insert(OPERATION.into(), Value::String("fetch".into()));
            fields.insert(SOURCE.into(), Value::String(source));
            fields.insert(DESTINATION.into(), Value::String(destination.symbolic()));
        }
        WorkspaceOperation::Copy {
            source,
            destination,
        } => {
            fields.insert(OPERATION.into(), Value::String("copy".into()));
            fields.insert(SOURCE.into(), Value::String(source.symbolic()));
            fields.insert(DESTINATION.into(), Value::String(destination.symbolic()));
        }
        WorkspaceOperation::Script { .. } | WorkspaceOperation::ArchiveCreate { .. } => {
            unreachable!("script/archive descriptors use their dedicated encodings")
        }
    }
    Value::Object(fields)
}

fn request_from_descriptor(value: &Value) -> Result<RelHostRequest, ModuleEvalError> {
    let Value::Object(fields) = value else {
        return Err(error("workspace.temp() requires a deferred host operation"));
    };
    let kind = string_arg(fields.get(MARKER), "host operation marker")?;
    match kind {
        "script" => script_request_from_fields(fields).map(RelHostRequest::Script),
        "archive" => archive_request_from_fields(fields).map(RelHostRequest::Archive),
        "workspace-op" => workspace_request_from_fields(fields).map(RelHostRequest::Workspace),
        _ => Err(error(format!(
            "workspace.temp() cannot execute deferred operation kind {kind:?}"
        ))),
    }
}

fn script_request_from_fields(fields: &HashMap<String, Value>) -> Result<ScriptPlan, ModuleEvalError> {
    let path = string_arg(fields.get(PATH), "deferred script path")?;
    let runtime = string_arg(fields.get("runtime"), "deferred script runtime")?;
    let mut plan = match runtime {
        "rbe.sys.nodejs" | "rbe.sys.bunjs" | "rbe.sys.python" => ScriptPlan::run(path),
        "rbe.sys.pypy" => ScriptPlan::run_pypy(path),
        "rbe.sys.rust" => ScriptPlan::run_rust(path),
        other => return Err(error(format!("unknown managed script runtime {other:?}"))),
    }
    .map_err(|e| error(e.to_string()))?;
    if let Some(Value::Array(args)) = fields.get(ARGS) {
        let mut parsed = Vec::with_capacity(args.len());
        for arg in args {
            let Value::String(arg) = arg else {
                return Err(error("deferred script args are invalid"));
            };
            parsed.push(arg.clone());
        }
        plan = plan.with_args(parsed).map_err(|e| error(e.to_string()))?;
    }
    if let Some(timeout) = fields.get(TIMEOUT_MS) {
        plan = plan
            .with_timeout_ms(positive_u64(timeout, "deferred script timeout")?)
            .map_err(|e| error(e.to_string()))?;
    }
    if plan.path.runtime_identity() != runtime {
        return Err(error("deferred script runtime does not match the validated path"));
    }
    Ok(plan)
}

fn archive_request_from_fields(fields: &HashMap<String, Value>) -> Result<ArchivePlan, ModuleEvalError> {
    let operation = string_arg(fields.get(OPERATION), "deferred archive operation")?;
    let plan = match operation {
        "list" => ArchivePlan::list(string_arg(fields.get(PATH), "archive path")?),
        "read" => ArchivePlan::read(
            string_arg(fields.get(PATH), "archive path")?,
            string_arg(fields.get(ENTRY), "archive entry")?,
        ),
        "extract" => {
            let Value::Array(entries) = fields
                .get(ENTRIES)
                .ok_or_else(|| error("deferred archive entries are missing"))?
            else {
                return Err(error("deferred archive entries are invalid"));
            };
            let mut parsed = Vec::with_capacity(entries.len());
            for entry in entries {
                let Value::String(entry) = entry else {
                    return Err(error("deferred archive entries are invalid"));
                };
                parsed.push(entry.clone());
            }
            ArchivePlan::extract(
                string_arg(fields.get(PATH), "archive path")?,
                parsed,
                string_arg(fields.get(DESTINATION), "archive destination")?,
            )
        }
        "create" => ArchivePlan::create(
            string_arg(fields.get(SOURCE), "archive source")?,
            string_arg(fields.get(DESTINATION), "archive destination")?,
        ),
        "replace" => ArchivePlan::replace(
            string_arg(fields.get(PATH), "archive path")?,
            string_arg(fields.get(ENTRY), "archive entry")?,
            string_arg(fields.get(SOURCE), "replacement source")?,
        ),
        "remove" => ArchivePlan::remove(
            string_arg(fields.get(PATH), "archive path")?,
            string_arg(fields.get(ENTRY), "archive entry")?,
        ),
        other => return Err(error(format!("unknown deferred archive operation {other:?}"))),
    };
    plan.map_err(|e| error(e.to_string()))
}

fn workspace_request_from_fields(
    fields: &HashMap<String, Value>,
) -> Result<WorkspacePlan, ModuleEvalError> {
    let operation = string_arg(fields.get(OPERATION), "deferred workspace operation")?;
    let mut plan = WorkspacePlan::construct();
    match operation {
        "fetch" => {
            let source = string_arg(fields.get(SOURCE), "workspace fetch source")?;
            let destination = WorkspacePath::parse(string_arg(
                fields.get(DESTINATION),
                "workspace fetch destination",
            )?)
            .map_err(|e| error(e.to_string()))?;
            plan.step(
                "operation",
                WorkspaceOperation::Fetch {
                    source: source.to_string(),
                    destination,
                },
            )
            .map_err(|e| error(e.to_string()))?;
        }
        "copy" => {
            let source = WorkspacePath::parse(string_arg(fields.get(SOURCE), "copy source")?)
                .map_err(|e| error(e.to_string()))?;
            let destination = WorkspacePath::parse(string_arg(
                fields.get(DESTINATION),
                "copy destination",
            )?)
            .map_err(|e| error(e.to_string()))?;
            plan.step("operation", WorkspaceOperation::Copy { source, destination })
                .map_err(|e| error(e.to_string()))?;
        }
        other => return Err(error(format!("unknown deferred workspace operation {other:?}"))),
    }
    Ok(plan)
}

fn expect_arity(name: &str, args: &[Value], expected: usize) -> Result<(), ModuleEvalError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(error(format!(
            "{name}() expects {expected} argument(s), got {}",
            args.len()
        )))
    }
}

fn string_arg<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, ModuleEvalError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(error(format!("{label} must be a non-empty string"))),
    }
}

fn positive_u64(value: &Value, label: &str) -> Result<u64, ModuleEvalError> {
    let Value::Number(value) = value else {
        return Err(error(format!("{label} must be a number")));
    };
    if !value.is_finite() || value.fract() != 0.0 || *value <= 0.0 || *value > u64::MAX as f64 {
        return Err(error(format!("{label} must be a positive integer")));
    }
    Ok(*value as u64)
}

fn error(message: impl Into<String>) -> ModuleEvalError {
    ModuleEvalError {
        code: "REL2200",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_run_is_deferred_and_runtime_is_inferred() {
        let value = deferred_call(
            "script",
            "run",
            vec![Value::String("$$/scripts/job.ts".into())],
        )
        .unwrap()
        .unwrap();
        let Value::Object(fields) = value else {
            panic!("expected descriptor")
        };
        assert!(matches!(fields.get(MARKER), Some(Value::String(value)) if value == "script"));
        assert!(matches!(fields.get("runtime"), Some(Value::String(value)) if value == "rbe.sys.bunjs"));
    }

    #[test]
    fn temp_wraps_deferred_script_without_running_it_early() {
        let value = deferred_call(
            "script",
            "runPyPy",
            vec![Value::String("??/heavy.py".into())],
        )
        .unwrap()
        .unwrap();
        let request = temp_request(&[value]).unwrap();
        assert!(matches!(request, RelHostRequest::Temp(_)));
    }

    #[test]
    fn archive_entries_reject_traversal_before_host_execution() {
        let error = deferred_call(
            "archive",
            "read",
            vec![
                Value::String("??/bundle.zip".into()),
                Value::String("../secret".into()),
            ],
        )
        .unwrap_err();
        assert_eq!(error.code, "REL2200");
    }
}
