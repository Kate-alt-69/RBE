# rbe-sdk

`rbe-sdk` is the stable Rust-side contract for external native RBE libraries.
It intentionally does **not** expose private `backend` structs or raw OS handles.
Libraries receive an RBE-supplied `HostBridge`; networking, router integration,
storage, crypto and future host functionality remain explicit capability-checked
calls across that bridge.

## Why this crate exists

A package such as `advancenet.zip` can build a higher-level networking library on
top of RBE's own `net` library without linking directly to private backend Rust
code:

```rust
use rbe_sdk::{AbiRange, HostBridge, LibraryDescriptor, RbeSdk};

fn perform_request(host: &dyn HostBridge, payload: &[u8]) {
    let descriptor = LibraryDescriptor {
        name: "advancenet",
        version: "1.0.0",
        abi: AbiRange::exact(1),
    };
    descriptor.validate().unwrap();

    let rbe = RbeSdk::new(host);
    let reply = rbe.net().http().call("request", payload).unwrap();
    // custom fallback/retry/cache logic can now consume `reply`.
}
```

The RBE library host remains the authority. If `advancenet` was not granted
`net:http`, the host rejects the call even though the SDK contains the helper.
The same rule applies to router registration, P2P listeners, storage, and every
other privileged surface.

## Easy by default, advanced by choice

The SDK is intentionally a **mechanism layer, not an opinionated application
framework**. Normal packages can stay on the tiny convenience API:

```rust
let reply = RbeSdk::new(host)
    .net()
    .http()
    .call("request", payload)?;
```

Advanced packages can instead create arbitrary capability/target clients, build
owned requests for queues or retries, batch independent calls, or wrap the raw
`HostBridge` with a completely custom abstraction:

```rust
let sdk = RbeSdk::new(host);

let encoder = sdk
    .capability_target("video:encode", "encoder:gpu0")
    .retarget("encoder:gpu1");
encoder.call("submit", frame)?;

let advanced = sdk.advanced();
let requests = [
    advanced
        .request("net:quic", "peer:primary", "connect")
        .payload(handshake),
    advanced
        .request("my-extension:cache", "cache:hot", "warm")
        .payload(seed),
];
let results = advanced.batch(&requests);
```

That open-ended surface is deliberate. The SDK does not try to predict every
future RBE feature or third-party extension and does not police package-level
business logic, retry strategy, caching model, or abstraction design. A package
can build something extremely high level or work almost directly against the
host protocol.

The boundary RBE *does* keep is authority: arbitrary SDK calls do not magically
grant capabilities. The host still validates the package identity, ABI,
capability grant, target, and operation before privileged work is accepted.

The JavaScript/TypeScript and Python bindings expose the same split through
`RbeSdk.capability(...)`, `RbeSdk.advanced()`, reusable requests, retargetable
capability clients, independent batch results, and direct bridge access.

## Self-hosted distribution

RBE's central package service is designed to expose a Cargo **sparse registry**
for SDK/tooling crates in addition to RBE's own package index. SDKs and runtimes
use the same user-facing install namespace as ordinary packages; there is no
separate `backend sdk ...` command family.

Target flow:

```text
./backend install sdk.0.1.0
./backend library new advancenet
./backend install advancenet
```

A normal package consumer does not need to install its SDK manually. The package
manifest declares the SDK requirement and `backend install` resolves the exact
compatible SDK automatically. Explicit `backend install sdk.<version>` is mainly
for authors preparing a local RBE library-development environment.

For manual Cargo use, a project can configure the same registry directly:

```toml
# .cargo/config.toml
[registries.rbe]
index = "sparse+https://<your-rbe-registry-host>/cargo/index/"
```

and then depend on the SDK normally:

```toml
[dependencies]
rbe-sdk = { version = "0.1", registry = "rbe" }
```

The package index and SDK registry are separate protocol surfaces backed by the
same central service. RBE packages are source ZIPs/native-library packages;
Cargo only needs the sparse registry for Rust SDK/tooling crates.

## ABI policy

`LIBRARY_ABI_VERSION` is independent from the SDK crate version. An SDK release
can add convenience helpers without forcing an ABI bump. Breaking changes to
the host/library wire contract require a new RBE library ABI.
