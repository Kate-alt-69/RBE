# `rbe-sdk` — Python

The Python SDK wraps RBE Library Protocol v1 and exposes the same capability IDs
as the Rust and JavaScript SDKs.

Normal projects should let Backend create the project-local Python environment
and package-index configuration:

```text
./backend sdk setup
```

Manual installation against a Kastrick-hosted Python Simple Repository remains
possible for SDK authors, but `backend install` is responsible for pinning the
runtime/SDK used by an RBE library package.

Example:

```python
from rbe_sdk import LibraryDescriptor, RbeSdk

DESCRIPTOR = LibraryDescriptor("advancenet_py", "1.0.0").validate()


def create_library(host_bridge):
    rbe = RbeSdk(host_bridge)
    return rbe.net().http()
```

The host bridge is supplied only after RBE verifies the package identity,
selected runtime and ABI handshake.
