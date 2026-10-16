# Versioned runtime binaries and standalone updates

RBE's source builds keep their existing compile-time administrator setup by default.
Use `./build.sh --deny-admin-setup` or `./build.ps1 --deny-admin-setup` to build a
runtime with **no embedded administrator password or verifier**. This flag skips
admin setup during compilation; it does not disable authentication. Container and
Service integrity bindings remain enabled.

## First boot and credential storage

A deferred build prompts for a password twice, without terminal echo, before normal
startup when stdin is a terminal. To initialize separately, run:

```sh
./backend --init-admin
```

Passwords must contain 12–1024 UTF-8 bytes. Unattended first boot accepts
`RBE_ADMIN_PASSWORD_FILE` (a UTF-8 secret file, optionally ending in a newline) or
`RBE_ADMIN_PASSWORD`. Prefer the secret file for service deployments. The password
environment variable is removed before backend workers start, and local password
buffers are zeroized after use. Delete the provisioned secret from the deployment's
configuration after successful setup. Existing credentials are never silently reset.

Only an Argon2id verifier is stored, at
`<binary directory>/data/admin/control-room/credentials.json`. Set `RBE_CONFIG_HOME`
to choose a persistent configuration directory instead; RBE appends `control-room/`.
Use the same location after every restart/update. Back it up privately. A damaged
record stops startup rather than accepting a replacement password.

Unix directories use mode 0700 and records mode 0600, with ownership and symlink
checks. Windows uses a protected DACL granting the current identity and SYSTEM access.
These controls protect against other OS users, not an administrator or a process
running as the same identity. The executable is never rewritten to store a password.
A headless deferred boot without credentials keeps the Control Room locked; it does
not create a default password or a public setup endpoint. Auto-update requires admin
initialization first.

When a build with an embedded verifier opts into updates, RBE saves that same
verifier as a protected PBKDF2 record before switching to a public deferred binary.
It refuses to overwrite a different existing record. Thus the existing password
continues working after the update.

## Binary tags, not GitHub Releases

`Runtime binary snapshots` builds these native bundles after source changes on
`main`, with PR builds validating the same matrix before publication:

| Rust target | CI host / compatibility baseline |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Ubuntu 22.04, glibc 2.35+ |
| `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM, glibc 2.39+ |
| `x86_64-pc-windows-msvc` | Windows x64, MSVC |
| `x86_64-apple-darwin` | macOS Intel runner |
| `aarch64-apple-darwin` | macOS Apple Silicon runner |

These are development builds, not a promise to support every Linux distribution
or older macOS version. Public ARM64 Linux bundles require the baseline above;
compile from source on older systems. macOS bundles are not Apple-notarized.

`VERSION` holds the manually selected base version, initially `0.1.0`. CI adds its
run number and attempt, such as `0.1.0-dev.42.1`, without editing source or Cargo
package versions. `backend --version` and `backend --version-json` expose the version,
source commit and target without opening listeners.

Only after **all five bundles succeed**, CI creates a separate parentless Git commit
containing `manifest.json` and `binaries/<target>/...`. It attaches an immutable tag,
for example `rbe-v0.1.0-dev.42.1`, and atomically advances `rbe-binaries-latest`.
Concurrent publishers use an expected-tip lease; an older run cannot rewind latest.
Publication never deletes source, changes `main`, or creates a GitHub Release.
Each tag adds binary Git objects to repository storage. Larger files are split into
80 MiB parts to stay below GitHub's per-file limit; this does not reduce total storage.

Use the downloader to reassemble all four files, checking sizes and SHA-256 hashes:

```sh
python3 binary-snapshots.py download --target x86_64-unknown-linux-gnu \
  --tag rbe-binaries-latest --destination ./rbe-runtime
```

Python 3.9+ and Git are needed for the publisher; downloading needs Python only.
Download into a **new** directory. It contains `backend`, `cloud_node`,
`dep/container`, `dep/service` (with `.exe` on Windows), plus `build-info.json`.
Bring your own `settings.json`, application REL sources and deployment data; these
are deliberately absent from the binary-only tag. Keep the complete matching bundle.
Run `python3 binary-snapshots.py -h` or a subcommand's `-h` for supported arguments.
An exact version tag pins a snapshot. Downloading GitHub's source ZIP for a binary
tag gives the parts; use this downloader to reconstruct large executables.

## Opt-in self-update

```sh
./backend --update-check
./backend --auto-update --settings settings.json
./backend --auto-update --update-tag rbe-v0.1.0-dev.42.1 --settings settings.json
```

Auto-update is off by default. It checks immediately and then every 15 minutes.
An exact tag pins the process; the default moving alias follows new builds. The
publisher repository is compiled into the binary (`RBE_BINARY_REPOSITORY`, default
`Kate-alt-69/RBE`), rather than taken from a remote request. Acquisition uses HTTPS
from that repository and validates the complete manifest, target, all sizes and
hashes, the candidate's reported identity, and protocol/credential compatibility.
Checksums detect corrupt or mixed downloads; they are **not an independent signature
against repository compromise**. Only install from a repository you trust.

The updater stages files under `.rbe-updates/`, then runs the candidate compiler
against the installed project's REL/RELC sources and settings **before shutdown**.
A rejected download or preflight leaves the active server running. The candidate
uses the existing project compilation pipeline, including disposable staging/cache
behavior; this is not a transactional data migration or an isolated copy of the app.

Once admitted, normal graceful shutdown drains the API and stops workers. A copy of
the old backend acts as the updater helper so Windows can release executable locks.
It waits for the parent's lifetime pipe to close, starts the maintenance responder,
journals the transaction, and replaces only the four fixed runtime files. Settings,
application sources, data and credentials stay in place. The replacement starts with
the original working directory, boot arguments and inherited environment.

The helper requires a matching process/version/source readiness record **and** a
successful `/health` response within 120 seconds. Launch or health failure stops the
candidate and restores the previous bundle, then restarts with auto-update disabled
so a bad build cannot loop. A successful update retains `.rbe-updates/previous/` until
the next admitted update. The helper supervises its child and forwards shutdown by
closing its lifetime pipe. Startup failures preserve their diagnostics.

A crash during file replacement leaves `.rbe-updates/transaction.json`. Normal boot
refuses that incomplete transaction. Run `backend --update-recover` to restore the
recorded bundle. If the backend file itself is missing, invoke a retained
`.rbe-updates/worker-*` with `--recover-installation <absolute installation directory>`.
Run recovery only after stopping any surviving backend/helper; the transaction lock
prevents concurrent updater work. Keep the journal and previous bundle if recovery
fails, and resolve permissions/locked processes before retrying.

Major-version, update-protocol and credential-format changes require manual
installation. Preflight catches compiler incompatibility, but cannot prove every
runtime behavior or reverse database/data migrations. Back up data before upgrades
that might change its format. Binary rollback restores executables only.

Render, systemd-managed and Kubernetes hosts reject in-place staging. Keep using
the hosting platform's deployment and rollback workflow there. Self-update is for
standalone installations with a writable binary directory; it is not a way to
bypass a service manager or update read-only containers.
