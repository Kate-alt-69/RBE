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

## OS-specific downloads and version tags

Open [the latest GitHub Release](https://github.com/Kate-alt-69/RBE/releases/latest)
and choose **only your operating system and architecture**. Windows gets a ZIP;
Linux and macOS get `.tar.gz` archives. You do not need to download every platform
or reconstruct Git binary parts for a normal installation.

| Download | Compatibility baseline |
| --- | --- |
| Windows x64 ZIP | Windows x64, MSVC |
| Linux x64 archive | Ubuntu 22.04, glibc 2.35+ |
| Linux ARM64 archive | Ubuntu 24.04 ARM, glibc 2.39+ |
| macOS Intel archive | macOS Intel runner |
| macOS Apple Silicon archive | macOS ARM64 runner |

Each archive contains its platform's complete matching runtime: `backend`,
`cloud_node`, `dep/container`, `dep/service` (with `.exe` on Windows), plus
`build-info.json`. Keep these files together. Settings, application REL files,
credentials and deployment data are not included. Extract into a new directory;
preserve existing configuration/data when upgrading. The Release lists SHA-256
checksums and the exact source commit/build revision.

Compile from source on systems older than the baselines above. macOS bundles are
not Apple-notarized.

`VERSION` selects the binary version, initially `0.1.0`. Use canonical
`major.minor.patch` numbers, such as `1.0.0`; padded components such as `1.00.0`
are rejected. Binary versions are separate from Cargo package and ABI versions.
`backend --version` and `backend --version-json` expose the version, source commit
and target without opening listeners.

| Tag | Behavior |
| --- | --- |
| `rbe-bin-v0.1.0` | Moves to the newest successful build of version `0.1.0`; its GitHub Release downloads refresh too |
| `rbe-bin-latest` | Moves to the newest successfully published build across versions |
| `rbe-bin-v0.1.0-build.42.1` | Immutable internal revision for run 42, attempt 1; supports verified acquisition and rollback |

Every push to `main` builds the native matrix. If `VERSION` is unchanged, a newer
source commit refreshes the **existing version's** tag and Release assets. To
start another numbered version, change `VERSION`, commit it with the intended
code, and merge or push to `main`. For example:

```sh
printf '0.1.1\n' > VERSION
git add VERSION
git commit -m "Select RBE binary version 0.1.1"
```

Versions cannot decrease. CI compares the version file before and after the whole
push, including multi-commit pushes. **Run workflow** can rebuild the current
source/version without a version bump. PR builds validate the matrix and do not
publish tags or Releases.

Only after **all five bundles and their checks succeed**, CI creates a separate
parentless Git commit containing `manifest.json` and `binaries/<target>/...`.
The immutable build tag points at that commit. Version and latest aliases move
atomically with expected-tip leases; an older run cannot rewind either alias.
Source `main` is never rewritten. Binary files over GitHub's per-file limit are
split into 80 MiB parts inside the Git tree.

Publishing then creates or refreshes the version's GitHub Release with one archive
per platform and a checksum file. Archives carry their build revision in the name.
All replacement assets upload and pass checksum verification before the Release's
download table changes; obsolete managed assets are removed afterward. Manually
uploaded files are preserved. Publishers are serialized so two builds cannot mix
Release updates. A failed upload keeps existing download links usable; retry the
publishing job to finish. Tags may already have advanced when an asset upload fails;
the updater uses the verified Git manifest independently of Release downloads.

The archives and binary Git history both consume GitHub storage. Planning fetches
only the source commits needed for the comparison, and publishing uses a shallow
source checkout; neither downloads accumulated binary tags. New archives do not
require a new version number when the source commit changes.

For an exact historical build, the Python downloader can reconstruct its verified
Git bundle without Git installed:

```sh
python3 binary-snapshots.py download --target x86_64-unknown-linux-gnu \
  --tag rbe-bin-v0.1.0-build.42.1 --destination ./rbe-runtime
```

Use `--tag rbe-bin-latest` or `--tag rbe-bin-v0.1.0` to download the current build
instead. Existing `rbe-v…-dev.…` tags remain accepted by the new downloader and
updater; they are not renamed or deleted. Older binaries whose updater only knows
the old prefix require one manual installation of a new bundle before following
`rbe-bin-latest`. Python 3.9+ is required for the downloader; publishing also uses
Git. Run a subcommand's `-h` for its arguments.

## Opt-in self-update

```sh
./backend --update-check
./backend --auto-update --settings settings.json
./backend --auto-update --update-tag rbe-bin-v0.1.1 --settings settings.json
```

Auto-update is off by default. It checks immediately and then every 15 minutes.
An immutable build tag pins the process; the default moving alias follows new builds. The
publisher repository is compiled into the binary (`RBE_BINARY_REPOSITORY`, default
`Kate-alt-69/RBE`), rather than taken from a remote request. Acquisition uses HTTPS
from that repository and validates the complete manifest, target, all sizes and
hashes, the candidate's reported identity, and protocol/credential compatibility.
Use `--update-tag rbe-bin-latest` (the default) to follow subsequent compatible
versions, or `--update-tag rbe-bin-v0.1.0` to receive newer commits within version
`0.1.0`. Use an immutable `-build.<run>.<attempt>` tag to stay on one exact build.
Rebuilds with the same version and a different source commit still pass through
the full preflight, maintenance, health-check and rollback path.
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
