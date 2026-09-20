# `@rbe/sdk` — Bun + Node.js

The JavaScript SDK is shared by Bun and Node.js. It wraps the same language-neutral
RBE Library Protocol used by Rust and Python; it does not grant capabilities by
being installed.

Normal projects should let Backend configure the project-local registry:

```text
./backend sdk setup
```

The generated scoped npm registry can then be consumed by either runtime:

```text
bun add @rbe/sdk
```

or:

```text
npm install @rbe/sdk
```

Example library code:

```js
import { libraryDescriptor, RbeSdk } from "@rbe/sdk";

export const descriptor = libraryDescriptor({
  name: "advancenet",
  version: "1.0.0"
});

export function createLibrary(hostBridge) {
  const rbe = new RbeSdk(hostBridge);
  return {
    async smartRequest(payload) {
      return rbe.net().http().call("request", payload);
    }
  };
}
```

The `hostBridge` is supplied by the RBE Library Host after the Protocol v1
handshake. External libraries do not create or choose that privileged channel.
