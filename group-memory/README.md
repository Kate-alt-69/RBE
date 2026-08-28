# RBE Group Memory

`rbe-group-memory` is a standalone, general-purpose memory-mapping crate for RBE.
It is not owned by the Engine, Service Mother, `.service`, Video Manager, or any
other single subsystem.

The first layer provides process-private anonymous mappings and file-backed
shared mappings with a versioned binary header, cooperative cross-process file
locking, and explicit flush control. File-backed regions intentionally rely on
the operating system's virtual-memory/page-cache implementation for physical
RAM versus disk residency.

Higher-level groups, named segments, leases/generations, and typed layouts will
build on this primitive without coupling the crate to a particular process
hierarchy.
