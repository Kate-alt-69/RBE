# OID compiler cache

Status: **implemented incrementally on RBE `main`**. The cache/index, dynamic linking, native Service assembly, Runtime Image pinning, and Vault-attested cache path exist; RELC continues to expand the set of REL operations that can be lowered natively.

OID means **Operation ID**. OIDs are an RBE compiler/link representation; they are **not** x86, ARM, or another CPU instruction set.

## Address space

The project-local OID address space is `u16` (`0..=65535`):

```text
0                  DONE (normal operation/function frame return)
1..5026            stable RBE operations
5027..20085        reserved RBE/compiler space
20086..30456       dynamic package/library operations
30457              END_PACKAGE (return from package sub-operation)
30458..65535       dynamic linked REL entities
```

`0` and `30457` are frame terminators. Neither means "kill service.exe".

## One index, sparse records

RELC owns exactly one project-local OID index:

```text
.cache/compiler/oid/index
```

That index defines the fixed address-space contract and records dynamic ownership/bindings. There is no second package OID index or manifest.

Only materialized operations receive physical files:

```text
.cache/compiler/oid/356
.cache/compiler/oid/20086
.cache/compiler/oid/30458
```

RBE does not create 65,536 files. Removed/unpinned dynamic assignments return to the reusable pool.

## RPX package identities

RPX exports stable symbolic library IDs such as:

```text
mail              -> lib_mail
mail.send         -> lib_mail_send
mail.Client.send  -> lib_mail_Client_send
```

RPX never publishes numeric package OIDs. RELC maps the verified export IDs to project-local OIDs inside the single index. Updating/removing a package lets RELC find the OIDs owned by the old package version, retire them, and assign available package-range OIDs to the new verified export surface.

Capabilities stay separate from identity: having an OID never grants a package permission.

## Native records and Service `.bin`

A sparse OID record can contain target-local machine-code bytes plus relocation/link metadata. RELC generates those fragments for the current build target. Service assembly then reads only the OIDs required by that Service, patches the RELC-provided relocations, and writes a disposable native cache artifact under the Service compiler cache.

Unsupported REL semantics remain explicit fallback; RBE must not generate fake machine code merely to make an OID look native.

## Diagnostics

Compiler/OID control values use the existing Error Book directly:

```text
-1 = ERROR
-2 = WARNING
-3 = UNKNOWN_FATAL
```

A rendered form may carry the existing code, for example `-1-REL2216` or another appropriate Error Book code. Negative diagnostic controls are not OIDs and do not consume the unsigned `0..65535` address space.

## Vault-attested cache security

`.cache/compiler` is disposable, but a running RBE must never execute arbitrary bytes merely because they are present there.

The secured OID path therefore uses:

```text
RELC generation
      |
      v
exclusive oid.lock
      |
      v
sparse OID records --SHA-256--> single OID index envelope
                                      |
                                      v
                                  Vault seal
                                      |
                                      v
                              trusted generation/head
```

The single protected index contains the record identities/digests. Vault owns the sealing/trusted-head authority. Tampering, swapping an OID into another slot, replaying an older index generation, or replacing bytes without the trusted Vault state must fail closed and force a verified rebuild rather than execution.

The lock protects concurrent writers; it is not itself the cryptographic security boundary.

## Runtime Image pinning

A Runtime Image pins the OID/index generation and native Service identities it was compiled against. Retired dynamic OIDs cannot be reused while a live image/worker still pins the old generation. This prevents package updates from rebinding an OID underneath active native code.

## Relationship to Library Host

Native OIDs are the fast compiled path. Library Host is the bounded external-runtime path. A package operation may eventually lower entirely to native code, or its native fragment may bridge to an approved managed runtime. The OID number itself never bypasses Library Host, Vault, package verification, or capability authorization.

See [`library-host.md`](library-host.md) and [`relc.md`](relc.md).
