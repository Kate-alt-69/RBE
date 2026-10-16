//! Global symbolic path helpers shared by REL host capabilities.

use crate::workspace::{WorkspacePath, WorkspacePlanError, WorkspaceRoot};

pub fn is_project_path(value: &str) -> bool {
    WorkspacePath::parse(value)
        .map(|path| path.root() == WorkspaceRoot::Project)
        .unwrap_or(false)
}

pub fn is_temp_path(value: &str) -> bool {
    WorkspacePath::parse(value)
        .map(|path| path.root() == WorkspaceRoot::Temp)
        .unwrap_or(false)
}

pub fn validate_symbolic_path(value: &str) -> Result<WorkspacePath, WorkspacePlanError> {
    WorkspacePath::parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_global_symbolic_roots() {
        assert!(is_project_path("$$/api/index.route"));
        assert!(is_temp_path("??/generated/task.ts"));
        assert!(!is_temp_path("/tmp/generated/task.ts"));
    }
}
