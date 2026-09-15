from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# BUG-STORAGE-001 staging cleanup: after capability validation is split into
# raw/public surfaces, RELC no longer needs the aggregate allowlist imports.
path = Path("engine/crates/route-engine/src/relc.rs")
text = path.read_text()
text = replace_once(
    text,
    """use crate::runtime_image::{
    stable_image_hash, stable_source_hash, storage_capability_operation_allowed,
    storage_capability_owner_allowed, storage_library_operation_allowed,
    storage_raw_operation_allowed, RuntimeCapabilityRequirement, RuntimeExecutable, RuntimeImage,
    RuntimeSourceManifest, STORAGE_CAPABILITY_OPERATIONS, STORAGE_LIBRARY_OPERATIONS,
    STORAGE_RAW_CAPABILITY_OPERATIONS,
};""",
    """use crate::runtime_image::{
    stable_image_hash, stable_source_hash, storage_capability_owner_allowed,
    storage_library_operation_allowed, storage_raw_operation_allowed, RuntimeCapabilityRequirement,
    RuntimeExecutable, RuntimeImage, RuntimeSourceManifest, STORAGE_LIBRARY_OPERATIONS,
    STORAGE_RAW_CAPABILITY_OPERATIONS,
};""",
    "RELC aggregate Storage import cleanup",
)
path.write_text(text)


# Rust 1.98 Clippy prefers fixed-size slice chunking through as_chunks. The
# UTF-16 decoder already rejects odd byte lengths before this block, so every
# pair is exact and can be converted without indexing.
path = Path("container-runtime/crates/container-runtime-core/src/storage_library.rs")
text = path.read_text()
text = replace_once(
    text,
    """    let units = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();""",
    """    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes(*chunk)
            } else {
                u16::from_be_bytes(*chunk)
            }
        })
        .collect::<Vec<_>>();""",
    "Storage UTF-16 fixed pair decoding",
)
path.write_text(text)
