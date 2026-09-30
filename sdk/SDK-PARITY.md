# RBE SDK parity contract

RBE ships one package SDK contract across Rust, JavaScript/TypeScript, and Python.
Language syntax may differ, but capability names, authority boundaries, and the normal helper surface must stay equivalent.

## Surface

Every SDK exposes these normal package helpers:

- `log(<library-name>)`
- `net()`
- `router()`
- `storage()`
- `crypto()`
- `host()`
- `capability(...)`
- `advanced()`

`advanced()` is intentionally the generic escape hatch. It is **not** a second hierarchy of typed helpers. It stays focused on generic capability/target/request/send/batch composition so packages can use future host capabilities before a convenience wrapper exists.

Typed built-in networking helpers live on `net()` in every SDK:

- `http()`
- `cookies()`
- `headers()`
- `url()`
- `dns()`
- `ip()`
- `tcp()`
- `udp()`
- `quic()`
- `websocket()`
- `webtransport()`
- `p2p()`
- `mask()`

These helpers do not bypass capability admission. They only remove repeated stringly-typed `net:*` construction from package code.

## Library logging

Package logging uses the host capability `log` and an explicit library target:

```text
lib/<verified-package-name>
```

For a package named `mail`:

```text
lib/mail
```

A child logger keeps the same authority target. Child scope is carried inside the structured log record instead of changing the capability target:

```text
lib/mail + scope ["smtp"] -> rendered module lib/mail/smtp
```

This is deliberate. A package cannot gain another package's log identity by changing a child name.

The supported operations are:

- `debug`
- `info`
- `warn`
- `error`
- `fatal`

The record payload is UTF-8 JSON with this shape:

```json
{
  "scope": ["smtp"],
  "message": "Listening on port 25"
}
```

The Library Host admits `log` automatically for exactly `lib/<verified-package-name>`. Logging therefore participates in the same HostBridge authorization path, filtering, structured output, and suppression behavior as other RBE host capabilities. Package code should not use `println!`, `console.log`, or direct stdout for operator logs.

### Rust

```rust
let log = sdk.log("mail")?;
log.info("Mail package initialized")?;
log.child("smtp")?.debug("Resolving MX records")?;

let dns = sdk.net().dns();
let tcp = sdk.net().tcp();
```

### JavaScript / TypeScript

```ts
const log = rbe.log("mail");
await log.info("Mail package initialized");
await log.child("smtp").debug("Resolving MX records");

const dns = rbe.net().dns();
const tcp = rbe.net().tcp();
```

### Python

```python
log = rbe.log("mail")
log.info("Mail package initialized")
log.child("smtp").debug("Resolving MX records")

dns = rbe.net().dns()
tcp = rbe.net().tcp()
```

## Authority rule

SDK convenience is never authority.

A package may construct a request for a capability, but the RBE Library Host still checks the exact admitted capability, target, operation, and payload limits. In particular, asking for `log` with `lib/another-package` is denied when the verified package is `mail`.

This document is the compatibility target when one language SDK changes: update the other SDK surfaces in the same work rather than allowing language-specific drift.
