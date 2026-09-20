# RBE Library Protocol v1

The RBE Library Protocol is the language-neutral wire contract between the RBE
Library Host and an external package worker. Rust, Python, Node.js, Bun, or an
`other` implementation all speak the same protocol. Language-specific SDKs are
convenience wrappers around this contract; they are not separate privilege
models.

## Transport

The Library Host starts the worker and provides a private inherited IPC endpoint
(pipe/socket/stdio bootstrap chosen by the host). The endpoint is not selected
by untrusted library code. Frames are length-bounded and versioned before any
capability request is accepted.

The exact binary framing is owned by RBE and may use the existing framed IPC
primitives. JSON examples below describe semantic fields, not a promise that the
wire representation itself will always be textual JSON.

## Handshake

The worker must identify what RBE is about to run before any host calls occur:

```json
{
  "type": "library.hello",
  "protocol": 1,
  "library": {
    "name": "advancenet",
    "version": "1.0.0",
    "contentHash": "sha256:..."
  },
  "sdk": {
    "language": "bun",
    "name": "@rbe/sdk",
    "version": "0.1.0"
  },
  "runtime": {
    "kind": "bun",
    "version": "1.x"
  },
  "abi": {
    "min": 1,
    "max": 1
  }
}
```

RBE checks the package identity, lockfile identity, ABI range, selected runtime,
installed SDK, declared exports, and granted capabilities against the package
manifest it admitted. A worker cannot broaden its manifest by announcing extra
capabilities during this handshake.

The host replies with an accept/reject frame containing the selected ABI and a
session-scoped capability identity. Only an accepted session can issue calls.

## Host capability call

Semantically:

```json
{
  "type": "host.call",
  "callId": 17,
  "capability": "net:http",
  "target": "net:http",
  "operation": "request",
  "payload": {}
}
```

Every call is checked against the package's admitted capability set. Possessing
an SDK helper does not grant its capability.

Router access is intentionally split:

- `router:read` for approved inspection/introspection;
- `router:register` for validated extension registration.

The same pattern applies to networking, storage, crypto, P2P and future host
surfaces.

## Worker lifetime

RBE owns worker start/stop/restart policy. A package cannot daemonize itself to
escape the Library Host. Children, filesystem access, environment access and
networking are governed by package/build/runtime policy rather than inherited
implicitly from Backend.

## Language identity

Supported first-party SDK families begin with:

- Rust: `rbe-sdk`
- JavaScript/TypeScript for Bun and Node.js: `@rbe/sdk`
- Python: `rbe-sdk`
- `other`: a package may implement Protocol v1 directly or use a third-party SDK
  for its language.

`other` is not permission to run an unknown binary blindly. `library.toml` must
still declare the runtime/toolchain, entry point, OS build/install instructions,
ABI range and resulting worker launch command.

## Cross-language compatibility

Host semantics, capability IDs, error codes and ABI negotiation are shared. A
Bun library and Rust library asking for `net:http` therefore reach the same RBE
host capability and receive the same authorization decision. SDKs may offer
language-native ergonomics, but they cannot invent more authority than the host
protocol exposes.
