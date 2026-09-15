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
