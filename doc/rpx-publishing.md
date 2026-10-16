# RPX authentication and publishing

RPX publishes RBE package archives through the Kastrick Package Index without accepting UAC passwords or private account IDs in the CLI.

## Registry configuration

RPX uses the same registry selection for install and publishing:

```powershell
$env:RPX_REGISTRY_URL = "https://<registry-host>/"
```

or per command:

```text
rpx --registry https://<registry-host>/ <command>
```

Production registries require HTTPS. Loopback HTTP is accepted only for local development.

## Login

```text
rpx login
```

The CLI requests a short-lived device authorization, prints the user code and verification URL, and best-effort opens the URL in the system browser. The user signs in through Kastrick/UAC and approves RPX there. RPX polls the device grant until it is approved or expires.

RPX stores only the scoped bearer token returned by the publisher. It never stores or receives the UAC private ID or registry owner key.

Default credential location:

```text
~/.rbe/rpx/auth.json
```

Override the file location with `RPX_AUTH_FILE`. For CI, a 64-hex scoped token may be supplied through `RPX_TOKEN`; environment credentials are never written into the credential store.

On Unix, RPX restricts the credential directory to `0700` and the token file to `0600`.

## Check the active publisher credential

```text
rpx whoami
```

This validates the credential against the registry and reports its source, expiry/scopes when locally known, and packages visible to that publisher. Internal UAC/owner identities are not printed.

## Logout

```text
rpx logout
```

RPX asks the registry to revoke the bearer token. Stored credentials are also removed locally. When the credential came from `RPX_TOKEN`, RPX cannot mutate the parent shell environment and tells the user to unset it.

## Publish

From an RBE package source tree:

```text
rpx publish
```

or:

```text
rpx publish ./path/to/package
```

The publish flow is:

```text
package.rbe.toml
      ↓
RPX package checks + managed compiler checks
      ↓
canonical .rbe.zip
  package.rbe.yaml
  .rbe/package-index.json
      ↓
POST /api/rpx/package/upload?version=<manifest-version>
  { "action": "prepare" }
      ↓
short-lived signed Supabase upload URL
      ↓
PUT archive bytes directly to storage
      ↓
POST /api/rpx/package/upload?version=<manifest-version>
  { "action": "publish", "uploadId": "..." }
      ↓
server verifies ownership + package identity + immutable release contract
```

The version is controlled by the URL query and must agree with the package manifest. RPX never sends a private UAC ID, owner key, or a client-selected ownership identity.

For first publication of an unused package name, the registry may atomically claim that package name for the authenticated publisher. Existing releases are immutable; publishing the same package/version again is rejected.

`rpx publish` accepts the same `--allow-host-toolchain` explicit local-authoring escape hatch as `rpx compile.package`. Managed compiler authority remains the default.

## Security properties

- No UAC password is handled by RPX.
- Device authorization expires server-side.
- Access tokens are registry-scoped and revocable.
- Private UAC identities and owner keys remain server-side.
- The public publish version comes only from `?version=`.
- Package archive bytes are uploaded directly to a short-lived signed storage URL rather than proxied through REL.
- RPX checks the final publication response matches the exact package name and version it built.
- Publisher/API responses are size-bounded and production endpoints require HTTPS.
