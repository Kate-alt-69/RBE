# rbe-auth plan

Status: proposed only. No `rbe-auth` package, RPX release, or REL import is implemented by this change.

## Goal

Provide a small authentication library that reuses existing authentication systems and lets developers build login/signup flows without rebuilding session handling. Authentication must remain usable without a supplied UI. An optional UI should support the application's own title, logo, colors, copy, and layout rather than requiring `KASTRICK | <service>` branding.

Implement the package in its own Python repository using the RBE Python SDK. It should register its own Service for account/session operations and durable data; the library API and optional UI call that authority. A self-hosted installation should be able to run its own authority with the same service/session/profile features. Kastrick is an explicit provider option for Kastrick accounts.

Branding and supported behavior should be configurable from the consuming `.module` or `.service`, with validated configuration shared by the Service and optional UI. Configuration must not allow credentials, signing keys, or private session tokens into browser-visible theme data.

The current Kastrick hosted web flow and desktop KA client are integration foundations, not this general-purpose library. Keep service/device tokens independently revocable: disconnecting one service invalidates its tokens and requires a fresh login there. Global profile edits go through the selected authority and propagate to its connected services.

## Proposed API surface

Agree on exact REL names after checking the package ABI. The initial concepts are:

- Create an auth client with an explicit provider and registered application/service identity.
- Start login or signup; handle callback state, cancellation, expiry, and provider errors.
- Read the current service-scoped session, sign out, and reconnect deliberately.
- Read/update the global profile through the selected authority, with revision conflicts preserved.
- Optionally mount accessible login/signup components with application theme options.
- Offer browser web and trusted desktop adapters separately. Web credentials stay server-side; desktop device approval uses the system browser and no application secret.

Applications should receive safe session/profile summaries. Account secrets, device polling secrets, provider credentials, and private session tokens must not cross into UI components or renderer IPC.

## RPX packaging and permission review

Publish an actual verified library artifact, pin its SDK/runtime/worker identity, test installation and REL imports, and declare only the RBE host privileges the chosen adapter needs. Package capability requests are not grants; use the existing project-local approval system bound to version, artifact hash, and lock hash.

Treat authentication as sensitive during design review. **A HIGH label is a proposed assessment, not a verified existing RPX classification.** Package requests are individually approved host grants, not general access to the host. Unsupported privileges must fail closed; do not invent capability names that appear to grant authority.

Use maintained implementations for protocol/crypto standards and existing auth/session code. Theme configuration must not widen registered callback origins, credential scopes, CSRF/state validation, or provider trust. Custom UI must clearly identify the account provider when users enter credentials or consent.

## Python and Service feasibility (source checked 2026-10-15)

- The Python SDK exposes `storage()` and `crypto()`. Backend dispatches those approved calls to package-scoped durable blob storage and bounded crypto operations, including secure randomness, SHA-256, HMAC-SHA256, constant-time comparison, and Ed25519 signing. See `sdk/python/rbe_sdk/__init__.py` and `engine/crates/backend/src/package_links/{storage,crypto}.rs`.
- Python packages accept both `python` and `pypy` runtime kinds. The install worker selects the matching managed runtime; a published, verified runtime artifact is still required. The supplied Render log validated zero managed runtime manifests, so PyPy installation on that deployment is not yet demonstrated.
- `service:package` supports verified package-owned `.service` sources under a package namespace (`install-runtime/src/package_service.rs`). Prove that a Python worker, its package Service, approved storage, and REL imports work together in an installed package before promising a complete auth runtime.
- Approved `net:http` and `net:tcp-listen` have host dispatch paths. The optional hosted UI still needs a tested route/callback integration; SDK method names alone are not evidence that every router capability is implemented.
- Host SHA-256 can hash image bytes within its payload limit. Image decoding, generated avatars, perceptual hashing, and password hashing are separate requirements. Host crypto has no password-KDF operation; use a maintained password-hashing implementation and verify its CPython/PyPy packaging, or add a reviewed host operation. SHA-256 is not a password hash.
- Durable blobs are a useful primitive, not proof of transactional account storage, safe concurrent updates, migrations, or secret-vault access. Specify and test those boundaries before storing production credentials.

## Delivery sequence

1. Verify the RPX publish → install → worker → REL import path for a minimal prebuilt library; separately resolve compiled-package build integration if required.
2. Inventory supported host permissions and the existing web/device/account APIs. Decide provider and secure-storage interfaces.
3. Specify typed login/signup/session/profile contracts and error/recovery states without implementing UI first.
4. Build a headless adapter, then optional themed accessible components and examples.
5. Validate cross-service isolation, callback/device replay, revocation, storage boundaries, upgrades, permission changes, and concurrent profile edits before publishing.

## RPX readiness evidence at planning time

Existing code covers registry resolution, SHA-pinned artifacts, application locks, publisher/device login, immutable publishing, verified worker launch, and explicit host permission approvals. `engine/crates/backend/src/install_cli.rs` now supports activation for graphs requiring no build steps; its older overview in `library-system.md` understates that progress. Packages requiring compilation still fail closed until the managed build executor is connected. `rel-host-capability-status.md` lists pending workspace/script/archive dispatch and executor integration. Several Developer Portal dashboard/package/release pages still import `lib/mock-data`.

These are concrete remaining boundaries, not a claim that every RPX feature is broken. A complete live registry-to-application acceptance run is still required before calling the whole library pipeline finished.
