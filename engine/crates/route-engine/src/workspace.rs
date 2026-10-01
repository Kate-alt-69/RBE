//! Workspace planning primitives shared by REL host capabilities.
//!
//! `$$/` remains the frozen project root. `??/` is an execution-scoped
//! temporary workspace root. Neither token is a raw host path: the trusted
//! Backend/Container host resolves them at execution time.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const PROJECT_ROOT_PREFIX: &str = "$$/";
pub const TEMP_ROOT_PREFIX: &str = "??/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkspaceRoot {
    Project,
    Temp,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkspacePath {
    root: WorkspaceRoot,
    relative: String,
}

impl WorkspacePath {
    pub fn parse(value: &str) -> Result<Self, WorkspacePlanError> {
        let (root, relative) = if let Some(relative) = value.strip_prefix(PROJECT_ROOT_PREFIX) {
            (WorkspaceRoot::Project, relative)
        } else if let Some(relative) = value.strip_prefix(TEMP_ROOT_PREFIX) {
            (WorkspaceRoot::Temp, relative)
        } else {
            return Err(WorkspacePlanError::InvalidPath(
                "workspace paths must begin with `$$/` or `??/`".into(),
            ));
        };

        validate_relative(relative)?;
        Ok(Self {
            root,
            relative: relative.replace('\\', "/"),
        })
    }

    pub fn project(relative: &str) -> Result<Self, WorkspacePlanError> {
        Self::parse(&format!("{PROJECT_ROOT_PREFIX}{relative}"))
    }

    pub fn temp(relative: &str) -> Result<Self, WorkspacePlanError> {
        Self::parse(&format!("{TEMP_ROOT_PREFIX}{relative}"))
    }

    pub fn root(&self) -> WorkspaceRoot {
        self.root
    }

    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn symbolic(&self) -> String {
        let prefix = match self.root {
            WorkspaceRoot::Project => PROJECT_ROOT_PREFIX,
            WorkspaceRoot::Temp => TEMP_ROOT_PREFIX,
        };
        format!("{prefix}{}", self.relative)
    }
}

fn validate_relative(value: &str) -> Result<(), WorkspacePlanError> {
    if value.is_empty() {
        return Ok(());
    }
    if value.starts_with('/') || value.starts_with('\\') {
        return Err(WorkspacePlanError::InvalidPath(
            "workspace-relative paths cannot be absolute".into(),
        ));
    }
    if value.contains('\0') {
        return Err(WorkspacePlanError::InvalidPath(
            "workspace paths cannot contain NUL".into(),
        ));
    }
    if value
        .replace('\\', "/")
        .split('/')
        .any(|segment| segment == "..")
    {
        return Err(WorkspacePlanError::InvalidPath(
            "workspace paths cannot traverse with `..`".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceOperation {
    Fetch {
        source: String,
        destination: WorkspacePath,
    },
    Script {
        path: WorkspacePath,
        args: Vec<String>,
    },
    ArchiveCreate {
        source: WorkspacePath,
        destination: WorkspacePath,
    },
    Copy {
        source: WorkspacePath,
        destination: WorkspacePath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedOperation {
    operation: WorkspaceOperation,
    dependencies: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspacePlan {
    operations: BTreeMap<String, PlannedOperation>,
}

impl WorkspacePlan {
    pub fn construct() -> Self {
        Self::default()
    }

    pub fn step(
        &mut self,
        name: impl Into<String>,
        operation: WorkspaceOperation,
    ) -> Result<&mut Self, WorkspacePlanError> {
        let name = normalized_step_name(name.into())?;
        if self.operations.contains_key(&name) {
            return Err(WorkspacePlanError::DuplicateStep(name));
        }
        self.operations.insert(
            name,
            PlannedOperation {
                operation,
                dependencies: BTreeSet::new(),
            },
        );
        Ok(self)
    }

    pub fn after(
        &mut self,
        step: &str,
        dependency: &str,
    ) -> Result<&mut Self, WorkspacePlanError> {
        if step == dependency {
            return Err(WorkspacePlanError::Cycle(vec![step.to_string()]));
        }
        if !self.operations.contains_key(dependency) {
            return Err(WorkspacePlanError::MissingStep(dependency.to_string()));
        }
        let Some(operation) = self.operations.get_mut(step) else {
            return Err(WorkspacePlanError::MissingStep(step.to_string()));
        };
        operation.dependencies.insert(dependency.to_string());
        self.validate_acyclic()?;
        Ok(self)
    }

    pub fn ready_batches(&self) -> Result<Vec<Vec<String>>, WorkspacePlanError> {
        self.validate_acyclic()?;
        let mut pending = self.operations.keys().cloned().collect::<BTreeSet<_>>();
        let mut completed = BTreeSet::<String>::new();
        let mut batches = Vec::new();

        while !pending.is_empty() {
            let ready = pending
                .iter()
                .filter(|name| {
                    self.operations
                        .get(*name)
                        .is_some_and(|step| step.dependencies.is_subset(&completed))
                })
                .cloned()
                .collect::<Vec<_>>();

            if ready.is_empty() {
                return Err(WorkspacePlanError::Cycle(
                    pending.iter().cloned().collect(),
                ));
            }
            for name in &ready {
                pending.remove(name);
                completed.insert(name.clone());
            }
            batches.push(ready);
        }
        Ok(batches)
    }

    pub fn operation(&self, name: &str) -> Option<&WorkspaceOperation> {
        self.operations.get(name).map(|step| &step.operation)
    }

    fn validate_acyclic(&self) -> Result<(), WorkspacePlanError> {
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for name in self.operations.keys() {
            self.visit(name, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    fn visit(
        &self,
        name: &str,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), WorkspacePlanError> {
        if visited.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            return Err(WorkspacePlanError::Cycle(vec![name.to_string()]));
        }
        let Some(step) = self.operations.get(name) else {
            return Err(WorkspacePlanError::MissingStep(name.to_string()));
        };
        for dependency in &step.dependencies {
            self.visit(dependency, visiting, visited)?;
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        Ok(())
    }
}

fn normalized_step_name(value: String) -> Result<String, WorkspacePlanError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(WorkspacePlanError::InvalidStepName(value.to_string()));
    }
    Ok(value.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspacePlanError {
    InvalidPath(String),
    InvalidStepName(String),
    DuplicateStep(String),
    MissingStep(String),
    Cycle(Vec<String>),
}

impl fmt::Display for WorkspacePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(message) => write!(f, "invalid workspace path: {message}"),
            Self::InvalidStepName(name) => write!(f, "invalid workspace step name {name:?}"),
            Self::DuplicateStep(name) => write!(f, "workspace step {name:?} already exists"),
            Self::MissingStep(name) => write!(f, "workspace step {name:?} does not exist"),
            Self::Cycle(names) => write!(f, "workspace dependency cycle: {}", names.join(" -> ")),
        }
    }
}

impl std::error::Error for WorkspacePlanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symbolic_roots_and_rejects_traversal() {
        assert_eq!(
            WorkspacePath::parse("$$/scripts/build.ts").unwrap().root(),
            WorkspaceRoot::Project
        );
        assert_eq!(
            WorkspacePath::parse("??/generated/output.zip").unwrap().root(),
            WorkspaceRoot::Temp
        );
        assert!(WorkspacePath::parse("??/../escape").is_err());
        assert!(WorkspacePath::parse("C:/escape").is_err());
    }

    #[test]
    fn batches_parallel_ready_operations() {
        let mut plan = WorkspacePlan::construct();
        plan.step(
            "repo",
            WorkspaceOperation::Copy {
                source: WorkspacePath::project("repo").unwrap(),
                destination: WorkspacePath::temp("repo").unwrap(),
            },
        )
        .unwrap();
        plan.step(
            "package",
            WorkspaceOperation::Script {
                path: WorkspacePath::project("scripts/package.ts").unwrap(),
                args: Vec::new(),
            },
        )
        .unwrap();
        plan.step(
            "docs",
            WorkspaceOperation::Script {
                path: WorkspacePath::project("scripts/docs.ts").unwrap(),
                args: Vec::new(),
            },
        )
        .unwrap();
        plan.after("package", "repo").unwrap();
        plan.after("docs", "repo").unwrap();
        let batches = plan.ready_batches().unwrap();
        assert_eq!(batches[0], vec!["repo"]);
        assert_eq!(batches[1], vec!["docs", "package"]);
    }
}
