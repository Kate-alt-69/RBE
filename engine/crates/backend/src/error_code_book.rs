//! Pre-bootstrap backend CLI routing and embedded RBE Error Code Book lookup.
//!
//! Public CLI commands must remain usable before settings, logging, Runtime
//! Image compilation, port binding, or child-process startup. The Error Code
//! Book is compiled into the backend so diagnostics remain available there too.

const CATALOG: &str = include_str!("../../../../doc/error-codes/catalog.json");
const REL: &str = include_str!("../../../../doc/error-codes/rel.md");
const RELC: &str = include_str!("../../../../doc/error-codes/relc.md");
const SERVICE: &str = include_str!("../../../../doc/error-codes/service.md");
const CONTAINER: &str = include_str!("../../../../doc/error-codes/container.md");
const RUNTIME: &str = include_str!("../../../../doc/error-codes/runtime.md");
const PUBLIC_BASE_URL: &str = "https://kastrick.vercel.app/project/rbe/doc/error-codes";

const GLOBAL_HELP: &str = r#"RBE backend

Usage:
  backend [boot options]
  backend <command> [arguments]
  backend install <target> [options]

Commands:
  check [--settings <file>]    Compile/link the application package and exit
  install <target> [options]   Install an RBE package, runtime, SDK, URL, or local archive
  help [command]               Show help for backend or a command

Diagnostics:
  --explain <CODE>             Explain an RBE error code
  --list-error-codes [PREFIX]  List registered RBE error codes

Boot examples:
  backend
  backend --settings settings.json
  backend -debug

Preflight example:
  backend check --settings settings.json

Help aliases:
  backend help
  backend -h
  backend --help
  backend -help

Run `backend help check` or `backend help install` for command syntax."#;

const CHECK_HELP: &str = r#"RBE package preflight

Usage:
  backend check
  backend check --settings <file>
  backend --settings <file> check

`check` loads settings, compiles the .service catalog, compiles and validates the
complete immutable Runtime Image, applies Server policy/middleware validation,
prints the resulting image identity/counts, and exits.

It runs before HostBootstrap and never binds the API port or starts Vault,
Container, Service workers, IPC listeners, or the maintenance responder. This is
intended for CI/build/deploy preflight and local REL diagnostics."#;

const INSTALL_HELP: &str = r#"RBE package installer

Usage:
  backend install <target> [options]
  backend install help

Target examples:
  advancenet
  advancenet.4.0.1
  runtime.python.3.10
  sdk.0.1.0
  https://example.com/rbe-index.json
  ./package.rbe-pkg

Options:
  -version <version> | --version <version>
  -shared
  -force
  -no-cache
  -refresh-index
  -json
  -quiet

Current build status:
  Public install command routing is active, but end-to-end package execution is
  not yet connected to backend.exe. A valid install request therefore exits
  with an unavailable error instead of starting the RBE server."#;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicCliDispatch {
    PassThrough,
    Check,
    Print(String),
    Fail { code: u8, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicCommandScan {
    PassThrough,
    GlobalHelp,
    Command(usize),
}

pub fn requested(args: &[String]) -> Option<anyhow::Result<String>> {
    match dispatch_public_cli(args) {
        PublicCliDispatch::PassThrough => {}
        PublicCliDispatch::Check => match run_package_check(args) {
            Ok(rendered) => return Some(Ok(rendered)),
            Err(error) => {
                eprintln!("RBE package check failed.\n\n{error:#}");
                std::process::exit(1);
            }
        },
        PublicCliDispatch::Print(text) => return Some(Ok(text)),
        PublicCliDispatch::Fail { code, message } => {
            eprintln!("{message}");
            std::process::exit(code.into());
        }
    }

    for (index, arg) in args.iter().enumerate() {
        if arg == "--explain" {
            let Some(code) = args.get(index + 1) else {
                return Some(Err(anyhow::anyhow!(
                    "--explain requires an error code, for example RELC3001"
                )));
            };
            return Some(explain(code));
        }
        if let Some(code) = arg.strip_prefix("--explain=") {
            return Some(explain(code));
        }
        if arg == "--list-error-codes" {
            let prefix = args
                .get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .map(String::as_str);
            return Some(list_codes(prefix));
        }
    }
    None
}

fn dispatch_public_cli(args: &[String]) -> PublicCliDispatch {
    match scan_public_command(args) {
        PublicCommandScan::PassThrough => PublicCliDispatch::PassThrough,
        PublicCommandScan::GlobalHelp => PublicCliDispatch::Print(GLOBAL_HELP.to_string()),
        PublicCommandScan::Command(index) => {
            let command = args[index].as_str();
            let command_args = &args[index + 1..];
            match command {
                "check" => dispatch_check(command_args),
                "help" => dispatch_help(command_args),
                "install" => dispatch_install(command_args),
                unknown => PublicCliDispatch::Fail {
                    code: 2,
                    message: format!(
                        "error: unknown backend command `{unknown}`\n\nRun `backend help` for usage."
                    ),
                },
            }
        }
    }
}

fn scan_public_command(args: &[String]) -> PublicCommandScan {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();

        if is_help(arg) {
            return PublicCommandScan::GlobalHelp;
        }

        // These flags own their complete invocation and are handled by the
        // existing pre-bootstrap/internal-mode code in main.rs.
        if is_internal_or_diagnostic_mode(arg) {
            return PublicCommandScan::PassThrough;
        }

        if !arg.starts_with('-') {
            return PublicCommandScan::Command(index);
        }

        if matches!(arg, "--settings") {
            if args.get(index + 1).is_none() {
                return PublicCommandScan::PassThrough;
            }
            index += 2;
            continue;
        }

        if arg.starts_with("--settings=")
            || matches!(arg, "--allow-settings-env" | "--debug" | "--debug-boot")
            || arg.starts_with("--debug-boot=")
            || arg.starts_with("-debug=")
        {
            index += 1;
            continue;
        }

        if arg == "-debug" {
            if args
                .get(index + 1)
                .is_some_and(|value| is_boolean_literal(value))
            {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        // Unknown leading options are left to the existing server/boot path so
        // this router does not accidentally reinterpret a legacy flag value as
        // a public command.
        return PublicCommandScan::PassThrough;
    }

    PublicCommandScan::PassThrough
}

fn dispatch_help(args: &[String]) -> PublicCliDispatch {
    let Some(command) = args.first().map(String::as_str) else {
        return PublicCliDispatch::Print(GLOBAL_HELP.to_string());
    };

    match command {
        "check" => PublicCliDispatch::Print(CHECK_HELP.to_string()),
        "install" => PublicCliDispatch::Print(INSTALL_HELP.to_string()),
        "help" => PublicCliDispatch::Print(GLOBAL_HELP.to_string()),
        unknown => PublicCliDispatch::Fail {
            code: 2,
            message: format!(
                "error: no help is available for unknown backend command `{unknown}`\n\nRun `backend help` for usage."
            ),
        },
    }
}

fn dispatch_check(args: &[String]) -> PublicCliDispatch {
    if args.first().is_some_and(|arg| arg == "help") || args.iter().any(|arg| is_help(arg)) {
        return PublicCliDispatch::Print(CHECK_HELP.to_string());
    }
    if let Err(message) = validate_check_options(args) {
        return PublicCliDispatch::Fail {
            code: 2,
            message: format!("error: {message}\n\n{}", check_usage()),
        };
    }
    PublicCliDispatch::Check
}

fn validate_check_options(args: &[String]) -> Result<(), String> {
    let mut index = 0usize;
    let mut settings_seen = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--settings" {
            if settings_seen {
                return Err("settings path was specified more than once".to_string());
            }
            settings_seen = true;
            index += 1;
            let Some(value) = args.get(index) else {
                return Err("--settings requires a file path".to_string());
            };
            if value.trim().is_empty() || value.starts_with('-') {
                return Err("--settings requires a file path".to_string());
            }
        } else if let Some(value) = arg.strip_prefix("--settings=") {
            if settings_seen {
                return Err("settings path was specified more than once".to_string());
            }
            settings_seen = true;
            if value.trim().is_empty() {
                return Err("--settings requires a file path".to_string());
            }
        } else if arg.starts_with('-') {
            return Err(format!("unknown check flag `{arg}`"));
        } else {
            return Err(format!("unexpected extra check argument `{arg}`"));
        }
        index += 1;
    }
    Ok(())
}

fn settings_path_from_args(args: &[String]) -> String {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--settings" {
            if let Some(value) = args.get(index + 1) {
                return value.clone();
            }
        }
        if let Some(value) = arg.strip_prefix("--settings=") {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    "settings.json".to_string()
}

fn run_package_check(args: &[String]) -> anyhow::Result<String> {
    let settings_path = settings_path_from_args(args);
    let mut config = config::Config::load(&settings_path)
        .map_err(|error| anyhow::anyhow!("failed to load {settings_path}: {error}"))?;
    let io = atomic_io::AtomicIo::new();
    let service_catalog = crate::service_boot::compile(&config.services, &io)?;
    let image = crate::runtime_image_boot::compile(&config, service_catalog.as_ref())?;
    crate::runtime_image_boot::apply_server_policy(&mut config, &image.server_policy)?;
    crate::runtime_image_boot::apply_middleware_plan(&mut config, &image.middleware_plan)?;
    Ok(format!(
        "RBE package check passed.\nsettings: {settings_path}\nruntimeImage: {}\nsourceHash: {}\nroutes: {}\nmodules: {}\nservices: {}",
        image.image_id,
        image.source_hash,
        image.routes.len(),
        image.modules.len(),
        image.services.len()
    ))
}

fn dispatch_install(args: &[String]) -> PublicCliDispatch {
    if args.first().is_some_and(|arg| arg == "help") || args.iter().any(|arg| is_help(arg)) {
        return PublicCliDispatch::Print(INSTALL_HELP.to_string());
    }

    let Some(target) = args.first().filter(|target| !target.starts_with('-')) else {
        return PublicCliDispatch::Fail {
            code: 2,
            message: format!(
                "error: backend install requires a target\n\n{}",
                install_usage()
            ),
        };
    };

    if let Err(message) = validate_install_options(&args[1..]) {
        return PublicCliDispatch::Fail {
            code: 2,
            message: format!("error: {message}\n\n{}", install_usage()),
        };
    }

    PublicCliDispatch::Fail {
        // 69 is EX_UNAVAILABLE: the request is valid, but this build does not
        // yet expose the end-to-end installer execution bridge.
        code: 69,
        message: format!(
            "error: `backend install` recognized target `{target}`, but package execution is not wired into backend.exe yet.\n\nNo RBE server was started and no project package state was changed.\nRun `backend help install` for the accepted install syntax."
        ),
    }
}

fn validate_install_options(args: &[String]) -> Result<(), String> {
    let mut index = 0usize;
    let mut version_seen = false;

    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(arg, "-version" | "--version") {
            if version_seen {
                return Err("version was specified more than once".to_string());
            }
            version_seen = true;
            index += 1;
            let Some(value) = args.get(index) else {
                return Err(format!("{arg} requires a value"));
            };
            validate_version(value)?;
        } else if let Some(value) = arg
            .strip_prefix("-version=")
            .or_else(|| arg.strip_prefix("--version="))
        {
            if version_seen {
                return Err("version was specified more than once".to_string());
            }
            version_seen = true;
            validate_version(value)?;
        } else if matches!(
            arg,
            "-shared"
                | "--shared"
                | "-force"
                | "--force"
                | "-no-cache"
                | "--no-cache"
                | "-refresh-index"
                | "--refresh-index"
                | "-json"
                | "--json"
                | "-quiet"
                | "--quiet"
        ) {
        } else if arg.starts_with('-') {
            return Err(format!("unknown install flag `{arg}`"));
        } else {
            return Err(format!("unexpected extra install argument `{arg}`"));
        }
        index += 1;
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<(), String> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!(
            "invalid version selector `{value}`; use major, major.minor, or major.minor.patch"
        ));
    }
    Ok(())
}

fn is_help(value: &str) -> bool {
    matches!(value, "-h" | "--help" | "-help")
}

fn is_internal_or_diagnostic_mode(value: &str) -> bool {
    matches!(
        value,
        "--maintenance-notice" | "--er" | "--vault" | "--explain" | "--list-error-codes"
    ) || value.starts_with("--explain=")
}

fn is_boolean_literal(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "0" | "true" | "false" | "yes" | "no" | "on" | "off"
    )
}

fn check_usage() -> &'static str {
    "Usage: backend check [--settings <file>]\nRun `backend help check` for details."
}

fn install_usage() -> &'static str {
    "Usage: backend install <target> [options]\nRun `backend help install` for details."
}

fn catalog() -> anyhow::Result<serde_json::Value> {
    serde_json::from_str(CATALOG)
        .map_err(|error| anyhow::anyhow!("embedded Error Code Book catalog is invalid: {error}"))
}

fn entries(catalog: &serde_json::Value) -> anyhow::Result<&Vec<serde_json::Value>> {
    catalog
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("embedded Error Code Book catalog has no entries"))
}

fn code_prefix(code: &str) -> String {
    code.chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn code_number(code: &str) -> Option<u32> {
    let digits = code
        .chars()
        .skip_while(|character| character.is_ascii_alphabetic())
        .collect::<String>();
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn code_matches_filter(code: &str, filter: &str) -> bool {
    let normalized = filter.trim().to_ascii_uppercase();
    if normalized
        .chars()
        .all(|character| character.is_ascii_alphabetic())
    {
        code_prefix(code) == normalized
    } else {
        code.starts_with(&normalized)
    }
}

fn render_entry(entry: &serde_json::Value) -> Option<String> {
    let code = entry.get("code")?.as_str()?;
    let status = entry
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("RBE diagnostic");
    Some(format!("{code:<10} [{status:<8}] {title}"))
}

fn render_known_codes(
    entries: &[serde_json::Value],
    prefix: Option<&str>,
    limit: Option<usize>,
) -> Vec<String> {
    let mut out = entries
        .iter()
        .filter(|entry| {
            let Some(code) = entry.get("code").and_then(serde_json::Value::as_str) else {
                return false;
            };
            prefix.is_none_or(|filter| code_matches_filter(code, filter))
        })
        .filter_map(render_entry)
        .collect::<Vec<_>>();
    out.sort();
    if let Some(limit) = limit {
        out.truncate(limit);
    }
    out
}

fn render_nearby_codes(
    entries: &[serde_json::Value],
    requested: &str,
    limit: usize,
) -> Vec<String> {
    let prefix = code_prefix(requested);
    let requested_number = code_number(requested);
    let mut ranked = entries
        .iter()
        .filter_map(|entry| {
            let code = entry.get("code")?.as_str()?;
            if code_prefix(code) != prefix {
                return None;
            }
            let distance = match (requested_number, code_number(code)) {
                (Some(requested), Some(candidate)) => requested.abs_diff(candidate),
                _ => u32::MAX,
            };
            Some((distance, code, render_entry(entry)?))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, rendered)| rendered)
        .collect()
}

pub fn list_codes(prefix: Option<&str>) -> anyhow::Result<String> {
    let catalog = catalog()?;
    let entries = entries(&catalog)?;
    let lines = render_known_codes(entries, prefix, None);
    if lines.is_empty() {
        let requested = prefix.unwrap_or("<all>").trim().to_ascii_uppercase();
        anyhow::bail!("no RBE error codes are registered for prefix {requested}");
    }
    let heading = prefix
        .map(|value| {
            format!(
                "RBE Error Code Book — {}",
                value.trim().to_ascii_uppercase()
            )
        })
        .unwrap_or_else(|| "RBE Error Code Book".to_string());
    Ok(format!("{heading}\n\n{}", lines.join("\n")))
}

pub fn explain(code: &str) -> anyhow::Result<String> {
    let requested = code.trim().to_ascii_uppercase();
    if requested.is_empty() {
        anyhow::bail!("error code cannot be empty");
    }

    let catalog = catalog()?;
    let entries = entries(&catalog)?;
    let Some(entry) = entries.iter().find(|entry| {
        entry
            .get("code")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(&requested))
    }) else {
        let suggestions = render_nearby_codes(entries, &requested, 8);
        if suggestions.is_empty() {
            anyhow::bail!(
                "unknown RBE error code {requested}; use --list-error-codes to inspect registered codes"
            );
        }
        anyhow::bail!(
            "unknown RBE error code {requested}. Nearby registered codes:\n{}",
            suggestions.join("\n")
        );
    };

    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("RBE diagnostic");
    let status = entry
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let doc = entry
        .get("doc")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Error Code Book entry {requested} has no doc target"))?;
    let (page_name, anchor) = doc.split_once('#').ok_or_else(|| {
        anyhow::anyhow!("Error Code Book entry {requested} has an invalid doc target")
    })?;
    let page = match page_name {
        "rel.md" => REL,
        "relc.md" => RELC,
        "service.md" => SERVICE,
        "container.md" => CONTAINER,
        "runtime.md" => RUNTIME,
        other => {
            anyhow::bail!("Error Code Book entry {requested} references unsupported page {other}")
        }
    };

    let marker = format!("<a id=\"{anchor}\"></a>");
    let start = page.find(&marker).ok_or_else(|| {
        anyhow::anyhow!("Error Code Book entry {requested} points to missing anchor {anchor}")
    })? + marker.len();
    let tail = &page[start..];
    let end = tail.find("\n<a id=\"").unwrap_or(tail.len());
    let section = tail[..end].trim();
    let body = section
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            if (index == 0 && line.starts_with("### ")) || line.starts_with("**Status:**") {
                None
            } else {
                Some(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    let online_page = page_name.strip_suffix(".md").unwrap_or(page_name);
    let online = format!("{PUBLIC_BASE_URL}/{online_page}#{anchor}");
    Ok(format!(
        "{requested} — {title}\nStatus: {status}\n\n{body}\n\nReference: doc/error-codes/{doc}\nOnline: {online}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn explains_specific_relc_error_from_embedded_book() {
        let rendered = explain("relc3001").expect("RELC3001 must be explainable");
        assert!(rendered.contains("RELC3001"));
        assert!(rendered.contains("Native Container execution required but lowering failed"));
        assert!(rendered.contains("doc/error-codes/relc.md#relc3001"));
        assert!(rendered
            .contains("https://kastrick.vercel.app/project/rbe/doc/error-codes/relc#relc3001"));
        assert_eq!(rendered.matches("RELC3001").count(), 1);
    }

    #[test]
    fn explains_service_error_from_embedded_book() {
        let rendered = explain("SVC5002").expect("SVC5002 must be explainable");
        assert!(rendered.contains("Service catalog changed"));
        assert!(rendered.contains("doc/error-codes/service.md#svc5002"));
        assert!(rendered
            .contains("https://kastrick.vercel.app/project/rbe/doc/error-codes/service#svc5002"));
    }

    #[test]
    fn unknown_code_suggests_nearest_same_subsystem_codes() {
        let error = explain("RELC3999").expect_err("unknown codes must be rejected");
        let message = error.to_string();
        assert!(message.contains("unknown RBE error code RELC3999"));
        assert!(message.contains("RELC4001"));
        assert!(message.contains("RELC3001"));
        assert!(!message.contains("REL1000"));
    }

    #[test]
    fn lists_codes_by_prefix() {
        let rendered = list_codes(Some("REL")).expect("REL code list must exist");
        assert!(rendered.contains("REL1000"));
        assert!(!rendered.contains("RELC1000"));
    }

    #[test]
    fn numeric_filter_can_narrow_a_family() {
        let rendered = list_codes(Some("SVC5")).expect("SVC5 range must exist");
        assert!(rendered.contains("SVC5001"));
        assert!(!rendered.contains("SVC1000"));
    }

    #[test]
    fn requested_accepts_equals_form() {
        let args = vec!["--explain=svc5002".to_string()];
        let rendered = requested(&args)
            .expect("lookup should be detected")
            .expect("lookup should succeed");
        assert!(rendered.contains("SVC5002"));
    }

    #[test]
    fn no_args_preserve_normal_server_boot() {
        assert_eq!(dispatch_public_cli(&[]), PublicCliDispatch::PassThrough);
    }

    #[test]
    fn ordinary_boot_flags_preserve_server_boot() {
        assert_eq!(
            dispatch_public_cli(&args(&["--settings", "custom.json"])),
            PublicCliDispatch::PassThrough
        );
        assert_eq!(
            dispatch_public_cli(&args(&["-debug"])),
            PublicCliDispatch::PassThrough
        );
    }

    #[test]
    fn internal_modes_remain_owned_by_existing_boot_code() {
        for values in [
            vec!["--er", "--launch"],
            vec!["--vault", "--separate-process"],
            vec!["--maintenance-notice"],
            vec!["--explain", "RELC3001"],
        ] {
            assert_eq!(
                dispatch_public_cli(&args(&values)),
                PublicCliDispatch::PassThrough
            );
        }
    }

    #[test]
    fn all_global_help_aliases_exit_before_boot() {
        for help in ["help", "-h", "--help", "-help"] {
            let PublicCliDispatch::Print(text) = dispatch_public_cli(&args(&[help])) else {
                panic!("{help} must render help");
            };
            assert!(text.contains("backend install <target>"));
            assert!(text.contains("backend check"));
        }
    }

    #[test]
    fn check_help_is_a_real_subcommand_help_path() {
        for values in [
            vec!["check", "help"],
            vec!["check", "--help"],
            vec!["help", "check"],
        ] {
            let PublicCliDispatch::Print(text) = dispatch_public_cli(&args(&values)) else {
                panic!("check help must render help");
            };
            assert!(text.contains("RBE package preflight"));
            assert!(text.contains("never binds the API port"));
        }
    }

    #[test]
    fn check_dispatches_before_server_boot_with_settings_on_either_side() {
        assert_eq!(
            dispatch_public_cli(&args(&["check", "--settings", "custom.json"])),
            PublicCliDispatch::Check
        );
        assert_eq!(
            dispatch_public_cli(&args(&["--settings", "custom.json", "check"])),
            PublicCliDispatch::Check
        );
        assert_eq!(
            settings_path_from_args(&args(&["check", "--settings=custom.json"])),
            "custom.json"
        );
    }

    #[test]
    fn check_rejects_unknown_flags_and_duplicate_settings() {
        assert!(matches!(
            dispatch_public_cli(&args(&["check", "--wat"])),
            PublicCliDispatch::Fail { code: 2, .. }
        ));
        assert!(matches!(
            dispatch_public_cli(&args(&[
                "check",
                "--settings",
                "one.json",
                "--settings=two.json"
            ])),
            PublicCliDispatch::Fail { code: 2, .. }
        ));
    }

    #[test]
    fn install_help_is_a_real_subcommand_help_path() {
        for values in [
            vec!["install", "help"],
            vec!["install", "--help"],
            vec!["help", "install"],
        ] {
            let PublicCliDispatch::Print(text) = dispatch_public_cli(&args(&values)) else {
                panic!("install help must render help");
            };
            assert!(text.contains("RBE package installer"));
        }
    }

    #[test]
    fn public_command_can_follow_known_global_boot_flags() {
        let PublicCliDispatch::Fail { code, message } = dispatch_public_cli(&args(&[
            "--settings",
            "custom.json",
            "-debug",
            "install",
            "mycoolpackage",
        ])) else {
            panic!("install must be dispatched before server boot");
        };
        assert_eq!(code, 69);
        assert!(message.contains("mycoolpackage"));
    }

    #[test]
    fn install_without_target_is_usage_error() {
        assert!(matches!(
            dispatch_public_cli(&args(&["install"])),
            PublicCliDispatch::Fail { code: 2, .. }
        ));
    }

    #[test]
    fn valid_install_request_fails_unavailable_without_booting_server() {
        let PublicCliDispatch::Fail { code, message } =
            dispatch_public_cli(&args(&["install", "mycoolpackage"]))
        else {
            panic!("install target must be intercepted");
        };
        assert_eq!(code, 69);
        assert!(message.contains("mycoolpackage"));
        assert!(message.contains("No RBE server was started"));
    }

    #[test]
    fn install_options_are_validated_before_unavailable_error() {
        assert!(matches!(
            dispatch_public_cli(&args(&["install", "demo", "--wat"])),
            PublicCliDispatch::Fail { code: 2, .. }
        ));
        assert!(matches!(
            dispatch_public_cli(&args(&["install", "demo", "-version", "3.10"])),
            PublicCliDispatch::Fail { code: 69, .. }
        ));
        assert!(matches!(
            dispatch_public_cli(&args(&[
                "install",
                "demo",
                "-version",
                "3.10",
                "--version=3.10"
            ])),
            PublicCliDispatch::Fail { code: 2, .. }
        ));
    }

    #[test]
    fn unknown_bare_word_is_a_command_error_not_server_boot() {
        let PublicCliDispatch::Fail { code, message } = dispatch_public_cli(&args(&["wat"])) else {
            panic!("unknown bare command must fail");
        };
        assert_eq!(code, 2);
        assert!(message.contains("unknown backend command"));
    }
}
