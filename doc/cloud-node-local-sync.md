# Cloud Node local directory + provider synchronization

Cloud Node can use a provider-backed history as a Git-like durable remote for one configured local directory. This mode does not require a second Cloud Node peer: the provider is the remote persistence/history endpoint and the local Cloud Node content-addressed store is the local repository state.

The canonical Cloud Node settings filename remains `setting.node.cn.json`. Provider credentials stay in environment variables; the settings file contains only provider configuration and credential environment-variable names.

## Model

```text
configured local directory
        |
        | scan changed/new/deleted files
        v
Cloud Node local CAS + history
        |
        | provider commits / immutable snapshot objects
        v
Supabase S3 / Amazon S3 / Azure / GCS / HTTP provider
```

`node.storageRoot` is Cloud Node's internal content-addressed store and backup/history area. It is **not** the directory being backed up. The watched directory is configured separately with `localSync.directory`.

Example:

```json
{
  "formatVersion": 1,
  "node": {
    "id": "render-main",
    "storageRoot": "./.rbe-cloud-node",
    "backupVersions": 5,
    "preserveOriginal": true,
    "videoChunkBytes": 4194304
  },
  "localSync": {
    "directory": "./data",
    "logicalPrefix": "workspace"
  },
  "provider": {
    "kind": "s3",
    "namespace": "production",
    "bucket": "kastrick-backend-storage",
    "endpoint": "https://PROJECT_REF.storage.supabase.co/storage/v1/s3",
    "region": "PROJECT_REGION",
    "auth": {
      "mode": "aws-sig-v4",
      "accessKeyEnv": "RBE_CN_PROV_SUPABASE_ACCESS_KEY",
      "secretKeyEnv": "RBE_CN_PROV_SUPABASE_SECRET_KEY"
    },
    "prefix": "rbe-cloud-node",
    "conflictPolicy": "fail",
    "autoReconnect": true,
    "syncOnConnect": true,
    "reconnectDelayMs": 2000,
    "maxReconnectDelayMs": 60000,
    "pollIntervalMs": 30000
  }
}
```

Relative `localSync.directory` paths are resolved relative to `setting.node.cn.json`. `logicalPrefix` defaults to `local` and names the Cloud Node logical namespace owned by this watched directory. Symlinks are skipped. Empty directories are not tracked, matching Git's file-oriented working-tree behavior.

If the Cloud Node internal store is nested below the watched directory, the scanner excludes that store so Cloud Node never recursively backs up its own CAS. Configuring the watched directory *inside* the Cloud Node internal store is rejected.

## Boot and synchronization order

Provider synchronization deliberately checks provider history **before local mutation**. This matters on ephemeral hosts such as Render and on a first launch after deleting the local Cloud Node cache.

A normal provider cycle is:

```text
1. Read LOCAL Cloud Node head/root and REMOTE provider head/root.
2. If REMOTE is ahead, pull/adopt provider history first.
3. If a provider pull occurred, materialize the pulled snapshot into localSync.directory.
4. Consume project Storage journal writes.
5. Scan localSync.directory and store changed/new files into Cloud Node.
6. Remove active Cloud Node objects for files deleted from the watched directory.
7. Reconcile provider history again and push the resulting descendant when LOCAL is ahead.
```

The provider remains authoritative for remote history during initial recovery. A fresh Cloud Node store with zero local objects and an existing provider history therefore pulls the provider state rather than manufacturing a new unrelated first commit.

After a pull, Cloud Node verifies its content-addressed store before materializing the working directory. File payloads are copied through verified temporary files before activation. A failed provider transfer never makes an unverified remote object become working-directory state.

## Backend-owned provider boot

A packaged RBE backend automatically owns the provider-mode Cloud Node lifecycle when a Cloud Node settings file is present beside the backend, or when `RBE_CN_SETTINGS` explicitly points at one.

For normal backend launches the order is:

```text
backend starts
  -> validate setting.node.cn.json
  -> provider + syncOnConnect=true:
       wait for `cloud_node sync`
       -> remote history recovery
       -> verified local CAS recovery
       -> localSync working-directory restore/reconciliation
  -> continue normal backend boot
  -> provider + autoReconnect=true:
       supervise `cloud_node run`
```

The initial `cloud_node sync` is blocking on purpose. If provider recovery was requested but fails, backend startup fails instead of allowing the application to boot against stale or empty state. This is especially important on ephemeral deployment hosts.

The supervised `cloud_node run` child is restarted with bounded exponential backoff if it exits unexpectedly. A provider daemon that remains healthy for at least 60 seconds resets the restart failure count.

`backend check`, help/validation invocations, and the separate Vault process do not auto-start provider synchronization. These commands must remain usable during build/preflight without requiring live provider credentials or network access.

The backend does not auto-start peer mode from this path. Provider mode is the no-second-node persistence path; the authenticated peer topology remains a separate Cloud Node deployment mode.

## Conflict behavior

`conflictPolicy` remains the explicit conflict authority:

- `fail` (default): when REMOTE advanced while `localSync.directory` contains uncommitted local changes, Cloud Node refuses to overwrite either side.
- `prefer-local`: local working-directory changes may intentionally become the winning provider history.
- `prefer-remote`: the provider history may intentionally replace local working-directory changes.

The default is deliberately conservative. Cloud Node does not silently choose one branch after both sides changed.

## Commands

### Full Git-like cycle

```text
cloud_node sync
```

With a provider this performs remote-first recovery, watched-directory reconciliation, and provider push/pull as one safe cycle.

With a peer it scans local inputs and performs the existing authenticated peer synchronization behavior.

### Continuous mode

```text
cloud_node run
```

When provider `syncOnConnect` is `true`, every provider poll performs the same full cycle as `sync`. Healthy provider polling uses `pollIntervalMs`; failures use the existing capped exponential reconnect backoff.

### Provider-only reconciliation

```text
cloud_node sync-provider
```

This command intentionally remains lower level: it reconciles only the Cloud Node CAS/provider history and does **not** scan or mutate `localSync.directory`. This preserves callers such as trusted registry bridges that need to recover provider state before importing their own authoritative snapshot.

### Inspect state

```text
cloud_node evaluate
cloud_node provider-status
cloud_node sync-plan
```

`evaluate` reports the configured watched directory, logical prefix, whether it differs from the local Cloud Node CAS, and the managed/scanned object counts.

## Supabase S3

Supabase's S3-compatible endpoint can be used through Cloud Node's existing S3 provider implementation. This retains SigV4 authentication and the S3 conditional-object semantics used by Cloud Node provider history.

Typical deployment values are:

```text
RBE_CN_PROV_SUPABASE_ACCESS_KEY=<S3 access key id>
RBE_CN_PROV_SUPABASE_SECRET_KEY=<S3 secret access key>
```

Do not store those credential values in `setting.node.cn.json`.

The watched directory, Cloud Node internal CAS, and Supabase provider are three distinct layers:

```text
working directory    = developer/application-visible files
node.storageRoot     = local Cloud Node CAS, backup versions, provider history
provider bucket      = remote immutable snapshots/history
```

That separation lets a fresh process rebuild local Cloud Node state from the provider, then safely rebuild the watched directory from the verified local CAS.
