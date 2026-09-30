use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn install(project: &Path) -> Result<PathBuf> {
    let rbe = project.join(".rbe");
    fs::create_dir_all(&rbe)
        .with_context(|| format!("create SDK activation directory: {}", rbe.display()))?;

    if cfg!(windows) {
        let path = rbe.join("activate.ps1");
        fs::write(&path, powershell_script(project)?)
            .with_context(|| format!("write SDK PowerShell activation: {}", path.display()))?;
        Ok(path)
    } else {
        let path = rbe.join("activate.sh");
        fs::write(&path, shell_script(project)?)
            .with_context(|| format!("write SDK shell activation: {}", path.display()))?;
        Ok(path)
    }
}

pub(crate) fn expected_path(project: &Path) -> PathBuf {
    if cfg!(windows) {
        project.join(".rbe").join("activate.ps1")
    } else {
        project.join(".rbe").join("activate.sh")
    }
}

fn powershell_script(project: &Path) -> Result<String> {
    let project = utf8_project(project)?;
    let quoted = powershell_single_quote(project);
    Ok(format!(
        r#"# RBE project-local command activation.
# This changes only the current PowerShell process. It never edits User or Machine PATH.
$global:RbeProjectRoot = [System.IO.Path]::GetFullPath('{quoted}').TrimEnd('\', '/')

function global:Assert-RbeProjectScope {{
    $cwd = [System.IO.Path]::GetFullPath((Get-Location).Path).TrimEnd('\', '/')
    $root = $global:RbeProjectRoot
    $separator = [System.IO.Path]::DirectorySeparatorChar
    if ($cwd -ne $root -and -not $cwd.StartsWith($root + $separator, [System.StringComparison]::OrdinalIgnoreCase)) {{
        throw "RBE project-local command is outside its owning project: $root"
    }}
}}

function global:backend {{
    param([Parameter(ValueFromRemainingArguments=$true)][object[]]$CommandArgs)
    Assert-RbeProjectScope
    & (Join-Path $global:RbeProjectRoot '.rbe\bin\backend.exe') @CommandArgs
}}

function global:rpx {{
    param([Parameter(ValueFromRemainingArguments=$true)][object[]]$CommandArgs)
    Assert-RbeProjectScope
    & (Join-Path $global:RbeProjectRoot '.rbe\bin\rpx.exe') @CommandArgs
}}

function global:Deactivate-RbeProject {{
    Remove-Item Function:\backend -ErrorAction SilentlyContinue
    Remove-Item Function:\rpx -ErrorAction SilentlyContinue
    Remove-Item Function:\Assert-RbeProjectScope -ErrorAction SilentlyContinue
    Remove-Item Function:\Deactivate-RbeProject -ErrorAction SilentlyContinue
    Remove-Variable RbeProjectRoot -Scope Global -ErrorAction SilentlyContinue
}}

Write-Host "RBE project commands activated for $global:RbeProjectRoot"
Write-Host "  backend / rpx are available only inside this project tree"
Write-Host "  run Deactivate-RbeProject to remove them from this shell"
"#
    ))
}

fn shell_script(project: &Path) -> Result<String> {
    let project = utf8_project(project)?;
    let quoted = shell_single_quote(project);
    Ok(format!(
        r#"# RBE project-local command activation.
# Source this file into the current shell. It never edits a user/system PATH file.
RBE_PROJECT_ROOT='{quoted}'
export RBE_PROJECT_ROOT

_rbe_require_project_scope() {{
    _rbe_pwd=$(pwd -P)
    case "$_rbe_pwd" in
        "$RBE_PROJECT_ROOT"|"$RBE_PROJECT_ROOT"/*) return 0 ;;
        *)
            echo "RBE project-local command is outside its owning project: $RBE_PROJECT_ROOT" >&2
            return 1
            ;;
    esac
}}

backend() {{
    _rbe_require_project_scope || return $?
    "$RBE_PROJECT_ROOT/.rbe/bin/backend" "$@"
}}

rpx() {{
    _rbe_require_project_scope || return $?
    "$RBE_PROJECT_ROOT/.rbe/bin/rpx" "$@"
}}

rbe_deactivate() {{
    unset -f backend rpx _rbe_require_project_scope rbe_deactivate 2>/dev/null || true
    unset RBE_PROJECT_ROOT
}}

echo "RBE project commands activated for $RBE_PROJECT_ROOT"
echo "  backend / rpx are available only inside this project tree"
echo "  run rbe_deactivate to remove them from this shell"
"#
    ))
}

fn utf8_project(project: &Path) -> Result<&str> {
    project
        .to_str()
        .context("SDK project path must be valid UTF-8 for shell activation")
}

fn powershell_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_activation_is_shell_local_and_scope_checked() {
        let script = powershell_script(Path::new(r"C:\work\mail")).unwrap();
        assert!(script.contains("function global:backend"));
        assert!(script.contains("function global:rpx"));
        assert!(script.contains("Assert-RbeProjectScope"));
        assert!(script.contains("outside its owning project"));
        assert!(!script.contains("$env:PATH"));
        assert!(!script.contains("SetEnvironmentVariable"));
    }

    #[test]
    fn shell_activation_is_shell_local_and_scope_checked() {
        let script = shell_script(Path::new("/work/mail")).unwrap();
        assert!(script.contains("backend()"));
        assert!(script.contains("rpx()"));
        assert!(script.contains("_rbe_require_project_scope"));
        assert!(script.contains("outside its owning project"));
        assert!(!script.contains("export PATH="));
    }

    #[test]
    fn shell_quoting_keeps_single_quotes_literal() {
        let script = shell_script(Path::new("/work/kat'e/mail")).unwrap();
        assert!(script.contains("kat'\"'\"'e"));
    }

    #[test]
    fn powershell_quoting_keeps_single_quotes_literal() {
        let script = powershell_script(Path::new(r"C:\work\kat'e\mail")).unwrap();
        assert!(script.contains("kat''e"));
    }
}
