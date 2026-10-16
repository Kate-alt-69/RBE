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

## Kastrick desktop accounts

The separate `@rbe/sdk/ka` subpath is a client for registered Kastrick desktop
applications. It does not change the Library Protocol host capability surface
and is not the proposed general-purpose `rbe-auth` RPX library. Use it in a trusted
Node.js main process with `fetch`, `AbortSignal.timeout`, and `AbortSignal.any`
support (Node.js 20.3+).

```js
import { KastrickAccountClient } from "@rbe/sdk/ka";

const account = new KastrickAccountClient({
	authority: "https://accounts.example",
	serviceId: "desktop-sketch",
	credentialStore: nativeCredentialVault
});

await account.signIn({
	onDevice: async ({ userCode, verificationUriComplete }) => {
		showVerificationCode(userCode);
		await openSystemBrowser(verificationUriComplete);
	}
});
const { profile } = await account.profile();
await account.updateProfile({
	displayName: "Ada",
	avatarTone: "cyan",
	expectedRevision: profile.revision
});
```

`nativeCredentialVault`, `showVerificationCode`, and `openSystemBrowser` are
application-owned adapters. The credential adapter must implement asynchronous
`read(key)`, `write(key, credential)`, and `remove(key)` using the OS credential
vault. No platform adapter is included here. Omitting it keeps sessions in main
process memory only. Never persist session/device secrets in renderer storage,
plaintext settings, logs, or IPC. Pass only the safe fields shown above to the UI.

`signIn` handles polling intervals/backoff and cancellation through `signal`.
For manual orchestration, `start` and `poll` are available to trusted main-process
code; they expose the private device code and must stay there. `resume` checks
local expiry/identity only; a subsequent `profile` checks the authority and clears
a rejected credential. Profile updates require the last-read revision; refresh
after a conflict. `signOut` revokes the current session and removes local storage.
`accountUrl` opens the central KA dashboard in the system browser.

Register the desktop service on kastrick-backend with `kind: "desktop"`; there is
no desktop client secret. See that repo's `doc/kastrick-desktop-account.md`. Test
this subpath with `node sdk/js/ka.test.mjs` from the RBE root. The future library
plan is in [`doc/rbe-auth-plan.md`](../../doc/rbe-auth-plan.md).
