from pathlib import Path
import json
import re


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one anchor, found {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1))


# ---- RELC runtime/compiler diagnostics ------------------------------------
path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()

old_enum = '''#[derive(Debug)]
pub enum RelcError {
    Registry(SourceRegistryError),
    Embedded(EmbeddedRelError),
    Server(ServerCompileError),
    Parse { source: SourceId, error: ParseError },
    RuntimeEnv(RuntimeEnvError),
    Policy(ServerPolicyError),
    Middleware(MiddlewarePlanError),
    Capability { source: SourceId, message: String },
    Link(String),
}
'''
new_enum = '''#[derive(Debug)]
pub enum RelcError {
    Registry(SourceRegistryError),
    Embedded(EmbeddedRelError),
    Server(ServerCompileError),
    Parse {
        source: SourceId,
        code: &'static str,
        error: ParseError,
    },
    RuntimeEnv(RuntimeEnvError),
    Policy(ServerPolicyError),
    Middleware(MiddlewarePlanError),
    Capability {
        source: SourceId,
        code: &'static str,
        message: String,
    },
    Link(String),
}
'''
if text.count(old_enum) != 1:
    raise SystemExit("relc.rs: RelcError enum anchor drifted")
text = text.replace(old_enum, new_enum, 1)

old_display = '''impl fmt::Display for RelcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "RELC source registry: {error}"),
            Self::Embedded(error) => write!(formatter, "{error}"),
            Self::Server(error) => write!(formatter, "{error}"),
            Self::Parse { source, error } => write!(
                formatter,
                "RELC parse error in {source} at {}:{}: {}",
                error.line, error.column, error.message
            ),
            Self::RuntimeEnv(error) => write!(formatter, "RELC Runtime ENV: {error}"),
            Self::Policy(error) => write!(formatter, "RELC ServerPolicy: {error}"),
            Self::Middleware(error) => write!(formatter, "RELC MiddlewarePlan: {error}"),
            Self::Capability { source, message } => {
                write!(formatter, "RELC capability error in {source}: {message}")
            }
            Self::Link(message) => write!(formatter, "RELC link error: {message}"),
        }
    }
}
'''
new_display = '''impl RelcError {
    /// Stable public diagnostic code. Broad migration codes intentionally
    /// remain broad until the originating compiler branch owns a narrower
    /// code; callers must never infer codes from English error text.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Registry(_) | Self::Embedded(_) | Self::Server(_) => "RELC1000",
            Self::Parse { code, .. } | Self::Capability { code, .. } => code,
            Self::RuntimeEnv(_) | Self::Policy(_) | Self::Middleware(_) => "RELC2200",
            Self::Link(_) => "RELC2000",
        }
    }

    /// Repository-relative long-form Error Code Book target.
    pub fn help_path(&self) -> String {
        let code = self.code();
        let book = if code.starts_with("RELC") {
            "relc.md"
        } else {
            "rel.md"
        };
        format!(
            "doc/error-codes/{book}#{}",
            code.to_ascii_lowercase()
        )
    }
}

impl fmt::Display for RelcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ", self.code())?;
        match self {
            Self::Registry(error) => write!(formatter, "RELC source registry: {error}"),
            Self::Embedded(error) => write!(formatter, "{error}"),
            Self::Server(error) => write!(formatter, "{error}"),
            Self::Parse { source, error, .. } => write!(
                formatter,
                "REL parse error in {source} at {}:{}: {}",
                error.line, error.column, error.message
            ),
            Self::RuntimeEnv(error) => write!(formatter, "RELC Runtime ENV: {error}"),
            Self::Policy(error) => write!(formatter, "RELC ServerPolicy: {error}"),
            Self::Middleware(error) => write!(formatter, "RELC MiddlewarePlan: {error}"),
            Self::Capability {
                source, message, ..
            } => write!(formatter, "RELC capability error in {source}: {message}"),
            Self::Link(message) => write!(formatter, "RELC link error: {message}"),
        }?;
        write!(formatter, "\\nhelp: {}", self.help_path())
    }
}
'''
if text.count(old_display) != 1:
    raise SystemExit("relc.rs: Display anchor drifted")
text = text.replace(old_display, new_display, 1)

old_lexer = '''        .map_err(|error| RelcError::Parse {
            source: id.clone(),
            error: ParseError {
'''
new_lexer = '''        .map_err(|error| RelcError::Parse {
            source: id.clone(),
            code: "REL1000",
            error: ParseError {
'''
if text.count(old_lexer) != 1:
    raise SystemExit("relc.rs: lexer Parse anchor drifted")
text = text.replace(old_lexer, new_lexer, 1)

old_parser = '''    result.map_err(|error| RelcError::Parse {
        source: id.clone(),
        error,
    })
'''
new_parser = '''    result.map_err(|error| RelcError::Parse {
        source: id.clone(),
        code: "REL1100",
        error,
    })
'''
if text.count(old_parser) != 1:
    raise SystemExit("relc.rs: parser Parse anchor drifted")
text = text.replace(old_parser, new_parser, 1)

pattern = re.compile(r'(RelcError::Capability \{\n)(\s*)(source:)')
text, capability_count = pattern.subn(
    lambda match: (
        match.group(1)
        + match.group(2)
        + 'code: "RELC2101",\n'
        + match.group(2)
        + match.group(3)
    ),
    text,
)
if capability_count != 8:
    raise SystemExit(f"relc.rs: expected 8 capability constructors, found {capability_count}")


def specialize_capability(fragment: str, code: str) -> None:
    global text
    index = text.find(fragment)
    if index < 0:
        raise SystemExit(f"relc.rs: missing capability fragment {fragment!r}")
    start = text.rfind("RelcError::Capability {", 0, index)
    if start < 0:
        raise SystemExit(f"relc.rs: fragment {fragment!r} is not in a capability error")
    code_index = text.find('code: "RELC2101"', start, index)
    if code_index < 0:
        raise SystemExit(f"relc.rs: no default code before {fragment!r}")
    text = text[:code_index] + f'code: "{code}"' + text[code_index + len('code: "RELC2101"'):]


specialize_capability("Environment Storage authority requires native Container execution;", "RELC3001")
specialize_capability("Storage namespace imports are forbidden;", "RELC2102")
specialize_capability("unsupported Environment Storage operation", "RELC2102")

old_namespace_test = '''        let error = compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
            .expect_err("Storage namespace import must fail closed");
        assert!(error.to_string().contains("exact operation"));
'''
new_namespace_test = '''        let error = compile_runtime_image("server Main {}", sources, &serde_json::json!({}))
            .expect_err("Storage namespace import must fail closed");
        assert_eq!(error.code(), "RELC2102");
        let rendered = error.to_string();
        assert!(rendered.contains("exact operation"));
        assert!(rendered.contains("help: doc/error-codes/relc.md#relc2102"));
'''
if text.count(old_namespace_test) != 1:
    raise SystemExit("relc.rs: storage namespace test anchor drifted")
text = text.replace(old_namespace_test, new_namespace_test, 1)

old_fallback_test = '''        let message = error.to_string();
        assert!(message.contains("Storage authority requires native Container execution"));
        assert!(message.contains("static JSON arguments"));
'''
new_fallback_test = '''        assert_eq!(error.code(), "RELC3001");
        let message = error.to_string();
        assert!(message.starts_with("RELC3001 "));
        assert!(message.contains("Storage authority requires native Container execution"));
        assert!(message.contains("static JSON arguments"));
        assert!(message.contains("help: doc/error-codes/relc.md#relc3001"));
'''
if text.count(old_fallback_test) != 1:
    raise SystemExit("relc.rs: storage fallback test anchor drifted")
text = text.replace(old_fallback_test, new_fallback_test, 1)

old_runtime_env_test = '''        assert!(matches!(
            compile_runtime_image(server, route, &serde_json::json!({})),
            Err(RelcError::Capability { .. })
        ));
'''
new_runtime_env_test = '''        let error = compile_runtime_image(server, route, &serde_json::json!({}))
            .expect_err("Route Runtime ENV must fail capability validation");
        assert_eq!(error.code(), "RELC2101");
        assert!(error
            .to_string()
            .contains("help: doc/error-codes/relc.md#relc2101"));
'''
if text.count(old_runtime_env_test) != 1:
    raise SystemExit(f"relc.rs: expected one Runtime ENV capability assertion, found {text.count(old_runtime_env_test)}")
text = text.replace(old_runtime_env_test, new_runtime_env_test, 1)

insert_anchor = '''mod tests {
    use super::*;

'''
insert_tests = '''mod tests {
    use super::*;

    #[test]
    fn rel_parser_failures_emit_stable_rel_code_and_help() {
        let routes = vec![PhysicalRelSource::new(
            RelSourceKind::Route,
            "broken",
            "api/broken.route",
            "class Route { get() {",
        )];
        let error = compile_runtime_image("server Main {}", routes, &serde_json::json!({}))
            .expect_err("invalid REL syntax must fail compilation");
        assert_eq!(error.code(), "REL1100");
        let rendered = error.to_string();
        assert!(rendered.starts_with("REL1100 "));
        assert!(rendered.contains("help: doc/error-codes/rel.md#rel1100"));
    }

    #[test]
    fn generic_relc_link_failures_emit_stable_code_and_help() {
        let error = RelcError::Link("example link failure".into());
        assert_eq!(error.code(), "RELC2000");
        let rendered = error.to_string();
        assert!(rendered.starts_with("RELC2000 "));
        assert!(rendered.contains("help: doc/error-codes/relc.md#relc2000"));
    }

'''
if text.count(insert_anchor) != 1:
    raise SystemExit("relc.rs: tests module anchor drifted")
text = text.replace(insert_anchor, insert_tests, 1)
path.write_text(text)

rel_path = Path("doc/error-codes/rel.md")
rel = rel_path.read_text()
anchor = "## Reserved migration codes\n"
if rel.count(anchor) != 1:
    raise SystemExit("rel.md: migration heading drifted")
rel = rel.replace(anchor, '''## Emitted migration umbrella codes

<a id="rel1000"></a>
### REL1000 — lexical error not yet classified more narrowly

**Status:** Emitted by RELC-linked compilation.

REL tokenization failed before a narrower stable lexer code had been assigned to that exact branch. The diagnostic still includes the original source location and lexer message.

**Action:** fix the reported tokenization problem. As individual lexer branches migrate, new releases may emit a more specific `REL1001+` code for the same class of source mistake.

<a id="rel1100"></a>
### REL1100 — syntax/parser error not yet classified more narrowly

**Status:** Emitted by RELC-linked compilation.

The REL parser rejected the source, but that parser branch has not yet been migrated to a narrower stable `REL11xx` code. This is a syntax-layer problem, not a RELC linking or capability error.

**Action:** use the reported line/column and parser message. Future releases may replace this umbrella code with a more specific `REL1101+` code without changing the underlying language rule.

## Reserved migration codes
''', 1)
rel_path.write_text(rel)

relc_path = Path("doc/error-codes/relc.md")
relc = relc_path.read_text()
anchor = "## Reserved migration codes\n"
if relc.count(anchor) != 1:
    raise SystemExit("relc.md: migration heading drifted")
relc = relc.replace(anchor, '''## Emitted migration umbrella codes

<a id="relc1000"></a>
### RELC1000 — source discovery/registration error not yet classified more narrowly

**Status:** Emitted.

RELC failed while discovering, extracting, registering, or initially compiling a source, but the originating branch does not yet own a narrower `RELC10xx` code.

<a id="relc2000"></a>
### RELC2000 — link/dependency error not yet classified more narrowly

**Status:** Emitted.

Whole-application linking failed before that branch had a narrower stable `RELC20xx` code. The original link reason remains in the diagnostic.

<a id="relc2200"></a>
### RELC2200 — Runtime ENV/policy/middleware lowering error

**Status:** Emitted.

RELC failed while resolving typed Runtime ENV, ServerPolicy, or MiddlewarePlan state used by the immutable Runtime Image.

## Specific migration codes
''', 1)

for heading in [
    "### RELC2101 — capability not permitted for source role",
    "### RELC2102 — capability target or operation is invalid",
]:
    old = heading + "\n\n**Status:** Reserved."
    new = heading + "\n\n**Status:** Emitted."
    if relc.count(old) != 1:
        raise SystemExit(f"relc.md: status anchor drifted for {heading}")
    relc = relc.replace(old, new, 1)

old = '''### RELC3001 — native Container execution required but lowering failed

**Status:** Reserved; current Storage-linked routes already fail closed on this condition with an unnumbered RELC capability error.
'''
new = '''### RELC3001 — native Container execution required but lowering failed

**Status:** Emitted.
'''
if relc.count(old) != 1:
    raise SystemExit("relc.md: RELC3001 status anchor drifted")
relc = relc.replace(old, new, 1)
relc_path.write_text(relc)

catalog_path = Path("doc/error-codes/catalog.json")
data = json.loads(catalog_path.read_text())
entries = data["entries"]
by_code = {entry["code"]: entry for entry in entries}


def insert_before(before_code: str, entry: dict) -> None:
    if entry["code"] in by_code:
        raise SystemExit(f"catalog already contains {entry['code']}")
    for index, current in enumerate(entries):
        if current["code"] == before_code:
            entries.insert(index, entry)
            by_code[entry["code"]] = entry
            return
    raise SystemExit(f"catalog insertion anchor {before_code} missing")


insert_before("REL1001", {"code":"REL1000","status":"emitted","title":"Lexical error not yet classified more narrowly","doc":"rel.md#rel1000"})
insert_before("REL1101", {"code":"REL1100","status":"emitted","title":"Syntax/parser error not yet classified more narrowly","doc":"rel.md#rel1100"})
insert_before("RELC1001", {"code":"RELC1000","status":"emitted","title":"Source discovery/registration error not yet classified more narrowly","doc":"relc.md#relc1000"})
insert_before("RELC2001", {"code":"RELC2000","status":"emitted","title":"Link/dependency error not yet classified more narrowly","doc":"relc.md#relc2000"})
insert_before("RELC3001", {"code":"RELC2200","status":"emitted","title":"Runtime ENV/policy/middleware lowering error","doc":"relc.md#relc2200"})
for code in ["RELC2101", "RELC2102", "RELC3001"]:
    by_code[code]["status"] = "emitted"

catalog_path.write_text(json.dumps(data, indent=2) + "\n")
