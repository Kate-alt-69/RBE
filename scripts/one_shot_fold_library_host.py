#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def read(rel: str) -> str:
    return (ROOT / rel).read_text(encoding="utf-8")


def write(rel: str, text: str) -> None:
    path = ROOT / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one anchor, found {count}")
    return text.replace(old, new, 1)


def remove_top_level_fn(text: str, name: str) -> str:
    marker = f"\nfn {name}("
    start = text.find(marker)
    if start < 0:
        return text
    candidates = [p for p in (text.find("\nfn ", start + len(marker)), text.find("\n#[cfg", start + len(marker))) if p >= 0]
    end = min(candidates) if candidates else len(text)
    return text[:start] + "\n" + text[end:]

# 1. Fold the old proxy executable entrypoint into container-bin itself.
cargo_rel = "container-runtime/crates/container-bin/Cargo.toml"
cargo = read(cargo_rel)
cargo = replace_once(
    cargo,
    '[[bin]]\nname = "container-library-worker-proxy"\npath = "src/library_worker_proxy_main.rs"\n\n',
    "",
    "container-bin Cargo proxy target",
)
write(cargo_rel, cargo)

old_mode_rel = "container-runtime/crates/container-bin/src/library_worker_proxy_main.rs"
new_mode_rel = "container-runtime/crates/container-bin/src/library_worker_mode.rs"
mode = read(old_mode_rel)
mode = replace_once(
    mode,
    "fn main() -> anyhow::Result<()> {\n    let args = std::env::args().skip(1).collect::<Vec<_>>();\n",
    "/// Run Container's internal Library Host execution mode.\n///\n/// The public artifact remains `container`; this mode is entered only through\n/// `container --library-worker-proxy` (or one of its private child flags).\npub fn run(args: &[String]) -> anyhow::Result<()> {\n",
    "Library Host mode entry",
)
mode = mode.replace("required_cgroup_path(&args)?", "required_cgroup_path(args)?")
mode = mode.replace("run_live_parent(&args)", "run_live_parent(args)")
mode = mode.replace("run_parent(&args)", "run_parent(args)")
write(new_mode_rel, mode)
(ROOT / old_mode_rel).unlink()

main_rel = "container-runtime/crates/container-bin/src/main.rs"
main = read(main_rel)
main = replace_once(
    main,
    "mod dashboard;\nmod environment_process;\n",
    "mod dashboard;\nmod environment_process;\nmod library_worker_mode;\n",
    "container module list",
)
main = replace_once(
    main,
    "fn main() -> anyhow::Result<()> {\n    tracing_subscriber::fmt::init();\n    let args = env::args().skip(1).collect::<Vec<_>>();\n\n",
    "fn main() -> anyhow::Result<()> {\n    let args = env::args().skip(1).collect::<Vec<_>>();\n\n    // Library Host execution is a Container mode, not a second packaged binary.\n    // Dispatch before normal Container initialization so stdout remains reserved\n    // for the strict Library Worker protocol frames.\n    if args.iter().any(|arg| arg == \"--library-worker-proxy\")\n        || args.iter().any(|arg| arg == \"--library-worker-exec-child\")\n        || args.iter().any(|arg| arg == \"--library-worker-live-child\")\n    {\n        return library_worker_mode::run(&args);\n    }\n\n    tracing_subscriber::fmt::init();\n\n",
    "container main dispatch",
)
write(main_rel, main)

# 2. Backend REL Host now invokes the already-attested Container artifact.
rel_rel = "engine/crates/backend/src/rel_host_executor.rs"
rel = read(rel_rel)
rel = rel.replace("use ed25519_dalek::{Signature, Verifier, VerifyingKey};\n", "")
rel = rel.replace("const PROXY_GRACE_SECONDS: u64 = 5;", "const LIBRARY_HOST_GRACE_SECONDS: u64 = 5;")
rel = re.sub(
    r'\nmod proxy_integrity \{\n    include!\(concat!\(\n        env!\("OUT_DIR"\),\n        "/library_worker_proxy_integrity.rs"\n    \)\);\n\}\n',
    "\n",
    rel,
    count=1,
)
rel = rel.replace("proxy_path: PathBuf,", "container_path: PathBuf,")
rel = replace_once(
    rel,
    "        let proxy_path = packaged_proxy_path()?;\n        verify_proxy(&proxy_path)?;\n",
    "        let container_path = super::ContainerProcess::packaged_path()?;\n        super::verify_container(&container_path)?;\n",
    "REL host packaged Container",
)
rel = rel.replace("            proxy_path,", "            container_path,")
rel = rel.replace("self.run_proxy(bootstrap).await?", "self.run_library_host(bootstrap).await?")
rel = rel.replace("    async fn run_proxy(\n", "    async fn run_library_host(\n")
rel = replace_once(
    rel,
    "        verify_proxy(&self.proxy_path).map_err(|error| {\n            rel_error(\n                \"REL2214\",\n                format!(\n                    \"packaged Container Library Worker Proxy failed integrity verification: {error}\"\n                ),\n            )\n        })?;\n",
    "        super::verify_container(&self.container_path).map_err(|error| {\n            rel_error(\n                \"REL2214\",\n                format!(\"packaged Container failed integrity verification before Library Host execution: {error}\"),\n            )\n        })?;\n",
    "REL host runtime verification",
)
rel = replace_once(
    rel,
    "        let mut command = Command::new(&self.proxy_path);\n        command\n            .arg(\"--cgroup-root\")\n",
    "        let mut command = Command::new(&self.container_path);\n        command\n            .arg(\"--library-worker-proxy\")\n            .arg(\"--cgroup-root\")\n",
    "REL host Container command",
)
rel = rel.replace("PROXY_GRACE_SECONDS", "LIBRARY_HOST_GRACE_SECONDS")
for old, new in {
    "could not seal Container proxy bootstrap": "could not seal Container Library Host bootstrap",
    "could not spawn verified Container Library Worker Proxy": "could not spawn verified Container Library Host mode",
    "Container proxy stdin pipe was not created": "Container Library Host stdin pipe was not created",
    "could not write Container proxy bootstrap": "could not write Container Library Host bootstrap",
    "could not close Container proxy bootstrap pipe": "could not close Container Library Host bootstrap pipe",
    "Container proxy exceeded its bounded outer timeout": "Container Library Host mode exceeded its bounded outer timeout",
    "Container proxy wait failed": "Container Library Host wait failed",
    "Container proxy returned an invalid result frame": "Container Library Host returned an invalid result frame",
    "proxy stderr": "Container stderr",
    "Container proxy completed without cgroup enforcement": "Container Library Host completed without cgroup enforcement",
    "Container proxy rejected script execution": "Container Library Host rejected script execution",
}.items():
    rel = rel.replace(old, new)

for name in ("packaged_proxy_path", "verify_proxy", "decode_exact", "signing_statement", "constant_time_eq"):
    rel = remove_top_level_fn(rel, name)
# sha256_file belonged only to the old separate-artifact verifier if it has no remaining caller.
if rel.count("sha256_file(") == 1:
    rel = remove_top_level_fn(rel, "sha256_file")
# Remove crypto imports only when the remaining source no longer needs them.
if "Sha256" not in rel:
    rel = rel.replace("use sha2::{Digest, Sha256};\n", "")
write(rel_rel, rel)

# 3. backend/build.rs binds only the one Container artifact.
build_rs_rel = "engine/crates/backend/build.rs"
build_rs = read(build_rs_rel)
build_rs = build_rs.replace(
    "//! The combined build compiles both `container-bin` and the dedicated\n//! `container-library-worker-proxy` first, then passes their exact outputs\n//! through `RBE_CONTAINER_BIN_PATH` and `RBE_LIBRARY_WORKER_PROXY_BIN_PATH`.\n//! This build script SHA-256 hashes and signs those exact bytes together with\n",
    "//! The combined build compiles the Container artifact first and passes its\n//! exact output through `RBE_CONTAINER_BIN_PATH`. Library Host execution is an\n//! internal `container --library-worker-proxy` mode, so no second executable or\n//! second integrity binding exists. This build script SHA-256 hashes and signs\n//! those exact Container bytes together with\n",
)
build_rs = build_rs.replace('    println!("cargo:rerun-if-env-changed=RBE_LIBRARY_WORKER_PROXY_BIN_PATH");\n', "")
build_rs = build_rs.replace('    let proxy_integrity_dest = Path::new(&out_dir).join("library_worker_proxy_integrity.rs");\n', "")
build_rs = re.sub(
    r'    let proxy_source = std::env::var\("RBE_LIBRARY_WORKER_PROXY_BIN_PATH"\)\n        \.ok\(\)\n        \.map\(PathBuf::from\);\n',
    "",
    build_rs,
    count=1,
)
build_rs = build_rs.replace(
    "    let signing_key = if source.is_some() || proxy_source.is_some() {\n",
    "    let signing_key = if source.is_some() {\n",
)
build_rs = re.sub(
    r'    let \(expected_proxy_hash, proxy_public_key, proxy_signature\) = signed_artifact\(\n        proxy_source,\n        "Library Worker Proxy binary",\n        "RBE-LIBRARY-WORKER-PROXY-INTEGRITY-V1",\n        signing_key\.as_ref\(\),\n        &build_id,\n        &target,\n    \);\n',
    "",
    build_rs,
    count=1,
)
build_rs = re.sub(
    r'\n    let proxy_literal = format!\(\n        "pub const EXPECTED_LIBRARY_WORKER_PROXY_SHA256: &str = .*?\n    fs::write\(&proxy_integrity_dest, proxy_literal\)\.unwrap_or_else\(\|err\| \{\n        panic!\(\n            "backend/build.rs: failed to write generated Library Worker Proxy integrity source: \{err\}"\n        \)\n    \}\);\n',
    "\n",
    build_rs,
    count=1,
    flags=re.S,
)
write(build_rs_rel, build_rs)

# 4. Secure release packaging now emits only dep/container for both normal and Library Host modes.
build_core_rel = "build-core.sh"
build_core = read(build_core_rel)
build_core = build_core.replace(
    "# Builds container-bin first, binds its exact bytes to backend.exe at build time,\n# and packages the same artifact as dist/<target>/dep/container(.exe).\n# The same container package build also emits the trusted Library Worker Proxy,\n# which is packaged beside Container for Backend-owned package sessions.\n",
    "# Builds container-bin first, binds its exact bytes to backend.exe at build time,\n# and packages the same artifact as dist/<target>/dep/container(.exe).\n# Library Host execution is an internal mode of that same attested Container binary.\n",
)
build_core = re.sub(
    r'    library_proxy_path=\$\(get_built_binary_path "\$CONTAINER_DIR" container-library-worker-proxy "\$target" "\$RELEASE"\)\n    \[ -f "\$library_proxy_path" \] \|\| \{ echo "ERROR: Library Worker Proxy artifact missing: \$library_proxy_path" >&2; exit 1; \}\n    library_proxy_dest="\$dep_dir/container-library-worker-proxy"; \[ "\$\(get_target_os "\$target"\)" = windows \] && library_proxy_dest="\$library_proxy_dest.exe"\n    cp "\$library_proxy_path" "\$library_proxy_dest"\n',
    "",
    build_core,
    count=1,
)
write(build_core_rel, build_core)

# 5. Preserve any Error Book namespace code (REL/SVC/CN/RBE/...) instead of falsely wrapping it in RBE5099.
backend_main_rel = "engine/crates/backend/src/main.rs"
backend_main = read(backend_main_rel)
old_classifier = '''fn has_rbe_error_code(details: &str) -> bool {
    details.lines().any(|line| {
        let Some(token) = line.split_whitespace().next() else {
            return false;
        };
        token.len() == 7
            && token.starts_with("RBE")
            && token.as_bytes()[3..].iter().all(u8::is_ascii_digit)
    })
}
'''
new_classifier = '''fn is_error_book_code(raw: &str) -> bool {
    let token = raw.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    let digit_start = token
        .find(|ch: char| ch.is_ascii_digit())
        .unwrap_or(token.len());
    let (namespace, digits) = token.split_at(digit_start);
    (2..=8).contains(&namespace.len())
        && namespace.chars().all(|ch| ch.is_ascii_uppercase())
        && digits.len() == 4
        && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn has_error_book_code(details: &str) -> bool {
    details.split_whitespace().any(is_error_book_code)
}
'''
backend_main = replace_once(backend_main, old_classifier, new_classifier, "boot fatal classifier")
backend_main = backend_main.replace("if has_rbe_error_code(&details) {", "if has_error_book_code(&details) {")
if "classified_rel_boot_error_is_not_wrapped_as_rbe5099" not in backend_main:
    backend_main += '''

#[cfg(test)]
mod boot_error_classification_tests {
    use super::*;

    #[test]
    fn classified_rel_boot_error_is_not_wrapped_as_rbe5099() {
        let error = anyhow::anyhow!("REL2216 trusted REL host executor could not be installed");
        let rendered = render_backend_boot_fatal(&error);
        assert!(rendered.starts_with("REL2216 "));
        assert!(!rendered.contains("RBE5099"));
    }

    #[test]
    fn unknown_boot_error_still_gets_rbe5099() {
        let error = anyhow::anyhow!("synthetic unknown boot failure");
        assert!(render_backend_boot_fatal(&error).starts_with("RBE5099 "));
    }
}
'''
write(backend_main_rel, backend_main)

# 6. Update existing Library Host docs away from the removed binary name.
for relpath in ("doc/library-host-web-build.md", "doc/library-worker-launch-preparation.md"):
    doc = read(relpath)
    doc = doc.replace("`container-library-worker-proxy`", "`container --library-worker-proxy`")
    doc = doc.replace("verified packaged container --library-worker-proxy", "verified packaged `container --library-worker-proxy`")
    write(relpath, doc)

library_host_doc = r'''# Library Host

Status: **implemented on RBE `main`**.

Library Host is RBE's trusted boundary for invoking verified package/library work that must execute through a managed external runtime such as Bun, Node.js, Python, or PyPy. It is **not** a second package manager, not a generic subprocess API, and not permission for REL code to spawn arbitrary host programs.

## Why it exists

RELC/Library Host may need to execute code that cannot be lowered directly into the in-process REL evaluator or the native OID path. Examples include an approved package worker or `script.run*` plan that targets one of RBE's managed `rbe.sys.*` runtimes.

The backend therefore prepares a sealed execution request while Container owns the operating-system sandbox boundary:

```text
verified package / REL request
          |
          v
Backend Library Host
  - resolve admitted rbe.sys runtime
  - bind runtime SHA-256
  - inventory source files + hashes
  - clear/limit arguments and environment
          |
          v
Container IPC bootstrap
          |
          v
dep/container --library-worker-proxy
  - re-verify runtime/source bytes
  - cgroup resource limits
  - no-new-privileges
  - Landlock workspace restriction where supported
  - restricted seccomp
  - no direct network by default
  - bounded stdout/stderr
  - bounded wall time
          |
          v
managed runtime process
```

## One Container binary

Library Host does **not** ship a separate `dep/container-library-worker-proxy` executable. The trusted entrypoint is an internal mode of the already-packaged and already-attested Container binary:

```text
dep/container --library-worker-proxy
```

Private child modes such as `--library-worker-exec-child` and `--library-worker-live-child` are implementation details used when Container establishes the sandbox. They are not public application APIs.

This matters for integrity: backend has one Container SHA-256/signature binding and re-verifies the same `dep/container` bytes before Library Host execution. There is no second sidecar binary and no second integrity metadata set that can drift away from the main Container artifact.

The IPC type names still use `LibraryWorkerProxy*` for protocol compatibility. In architecture/documentation, **Container Library Host mode** is the preferred name for the execution boundary.

## Trust rules

Library Host fails closed when any required identity or sandbox primitive cannot be established. In particular:

- no PATH fallback for managed runtimes;
- no shell execution for ordinary Library Host plans;
- runtime executable identity is pinned before launch;
- source files are inventoried and hashed before launch and checked again at the Container boundary;
- direct network is denied unless a future capability explicitly authorizes it;
- cgroup enforcement is required on the supported Linux execution path;
- output and execution time are bounded;
- Container is launched from the packaged `dep/container` path and must match backend's build-time integrity binding.

`REL2216` means the trusted REL host executor itself could not be installed. Because `REL2216` is already an Error Book code, backend boot must preserve it as the primary diagnostic rather than relabeling it as generic `RBE5099`.

## Relationship to RPX

RPX determines the verified package graph/artifacts. Library Host does not change package identity or grant capabilities. It consumes an already-verified package/runtime/source plan and gives it a bounded execution boundary.

## Relationship to OIDs

OID-native Service work and Library Host are complementary:

- an operation that RELC successfully lowers to a native OID does not need Library Host just to execute that operation;
- package/library work that still requires Bun/Node/Python/etc. can use a package OID whose native fragment bridges into an approved Library Host/runtime path;
- OID allocation never grants package capability by itself;
- the OID cache remains project-local compiler cache state, while Library Host remains an execution/sandbox authority boundary.

See [`oid-compiler-cache.md`](oid-compiler-cache.md), [`library-system.md`](library-system.md), and [`library-host-web-build.md`](library-host-web-build.md).
'''
write("doc/library-host.md", library_host_doc)

oid_doc = r'''# OID compiler cache

Status: **implemented incrementally on RBE `main`**. The cache/index, dynamic linking, native Service assembly, Runtime Image pinning, and Vault-attested cache path exist; RELC continues to expand the set of REL operations that can be lowered natively.

OID means **Operation ID**. OIDs are an RBE compiler/link representation; they are **not** x86, ARM, or another CPU instruction set.

## Address space

The project-local OID address space is `u16` (`0..=65535`):

```text
0                  DONE (normal operation/function frame return)
1..5026            stable RBE operations
5027..20085        reserved RBE/compiler space
20086..30456       dynamic package/library operations
30457              END_PACKAGE (return from package sub-operation)
30458..65535       dynamic linked REL entities
```

`0` and `30457` are frame terminators. Neither means "kill service.exe".

## One index, sparse records

RELC owns exactly one project-local OID index:

```text
.cache/compiler/oid/index
```

That index defines the fixed address-space contract and records dynamic ownership/bindings. There is no second package OID index or manifest.

Only materialized operations receive physical files:

```text
.cache/compiler/oid/356
.cache/compiler/oid/20086
.cache/compiler/oid/30458
```

RBE does not create 65,536 files. Removed/unpinned dynamic assignments return to the reusable pool.

## RPX package identities

RPX exports stable symbolic library IDs such as:

```text
mail              -> lib_mail
mail.send         -> lib_mail_send
mail.Client.send  -> lib_mail_Client_send
```

RPX never publishes numeric package OIDs. RELC maps the verified export IDs to project-local OIDs inside the single index. Updating/removing a package lets RELC find the OIDs owned by the old package version, retire them, and assign available package-range OIDs to the new verified export surface.

Capabilities stay separate from identity: having an OID never grants a package permission.

## Native records and Service `.bin`

A sparse OID record can contain target-local machine-code bytes plus relocation/link metadata. RELC generates those fragments for the current build target. Service assembly then reads only the OIDs required by that Service, patches the RELC-provided relocations, and writes a disposable native cache artifact under the Service compiler cache.

Unsupported REL semantics remain explicit fallback; RBE must not generate fake machine code merely to make an OID look native.

## Diagnostics

Compiler/OID control values use the existing Error Book directly:

```text
-1 = ERROR
-2 = WARNING
-3 = UNKNOWN_FATAL
```

A rendered form may carry the existing code, for example `-1-REL2216` or another appropriate Error Book code. Negative diagnostic controls are not OIDs and do not consume the unsigned `0..65535` address space.

## Vault-attested cache security

`.cache/compiler` is disposable, but a running RBE must never execute arbitrary bytes merely because they are present there.

The secured OID path therefore uses:

```text
RELC generation
      |
      v
exclusive oid.lock
      |
      v
sparse OID records --SHA-256--> single OID index envelope
                                      |
                                      v
                                  Vault seal
                                      |
                                      v
                              trusted generation/head
```

The single protected index contains the record identities/digests. Vault owns the sealing/trusted-head authority. Tampering, swapping an OID into another slot, replaying an older index generation, or replacing bytes without the trusted Vault state must fail closed and force a verified rebuild rather than execution.

The lock protects concurrent writers; it is not itself the cryptographic security boundary.

## Runtime Image pinning

A Runtime Image pins the OID/index generation and native Service identities it was compiled against. Retired dynamic OIDs cannot be reused while a live image/worker still pins the old generation. This prevents package updates from rebinding an OID underneath active native code.

## Relationship to Library Host

Native OIDs are the fast compiled path. Library Host is the bounded external-runtime path. A package operation may eventually lower entirely to native code, or its native fragment may bridge to an approved managed runtime. The OID number itself never bypasses Library Host, Vault, package verification, or capability authorization.

See [`library-host.md`](library-host.md) and [`relc.md`](relc.md).
'''
write("doc/oid-compiler-cache.md", oid_doc)

readme_rel = "doc/README.md"
readme = read(readme_rel)
readme = replace_once(
    readme,
    "- [`library-system.md`](library-system.md) — external libraries and the current project package/install architecture, including `package.rbe.yaml`, lockfile activation, managed `rbe.sys.*` tools, package attestation, durable install sessions, and controlled dependency hydration.\n",
    "- [`library-system.md`](library-system.md) — external libraries and the current project package/install architecture, including `package.rbe.yaml`, lockfile activation, managed `rbe.sys.*` tools, package attestation, durable install sessions, and controlled dependency hydration.\n- [`library-host.md`](library-host.md) — Library Host purpose, trust boundary, and the internal `container --library-worker-proxy` execution mode.\n- [`oid-compiler-cache.md`](oid-compiler-cache.md) — OID address space, one-index layout, RPX bindings, native Service assembly, diagnostics, and Vault-attested cache security.\n",
    "doc README links",
)
readme = readme.replace(
    "Verified Library Host launch and managed web-build execution now have an implemented path through the current sealed worker proof + Container proxy/sandbox work;",
    "Verified Library Host launch and managed web-build execution now have an implemented path through the current sealed worker proof + Container Library Host mode/sandbox work;",
)
old_name = "- **Library Worker Proxy** — the trusted Container-side proxy that re-verifies sealed library worker launch inputs, establishes the bounded sandbox, and emits proxy readiness before Library Protocol traffic begins."
new_name = "- **Container Library Host mode** — the internal `container --library-worker-proxy` path that re-verifies sealed library worker launch inputs and establishes the bounded sandbox. `LibraryWorkerProxy*` remains the internal IPC protocol naming; there is no separate packaged proxy executable."
readme = replace_once(readme, old_name, new_name, "doc README Library Host naming")
write(readme_rel, readme)

# Fail if the removed executable name remains in build/runtime/docs. Internal protocol type names are intentionally retained.
for relpath in (
    cargo_rel,
    build_core_rel,
    build_rs_rel,
    rel_rel,
    "doc/library-host-web-build.md",
    "doc/library-worker-launch-preparation.md",
    "doc/library-host.md",
    readme_rel,
):
    if "container-library-worker-proxy" in read(relpath):
        raise SystemExit(f"{relpath}: removed proxy executable name still present")

print("Library Host Container-mode refactor staged successfully")
