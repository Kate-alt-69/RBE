from pathlib import Path


def rep(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding='utf-8')
    found = text.count(old)
    if found != count:
        raise SystemExit(f'{path}: expected {count} anchors, found {found}: {old[:120]!r}')
    p.write_text(text.replace(old, new, count), encoding='utf-8')

runtime = 'engine/crates/route-engine/src/runtime_image.rs'
rep(runtime,
'''use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::ast''',
'''use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};

use crate::ast''')
rep(runtime, '    pub source_hash: u64,', '    pub source_hash: String,')

start = text_start = '''pub(crate) fn stable_source_hash<'a>(
    sources: impl Iterator<Item = (&'a SourceId, &'a str)>,
) -> u64 {
    // Explicit FNV-1a avoids relying on std's non-contractual DefaultHasher
    // algorithm for Runtime Image identity.
    let mut hash = 0xcbf29ce484222325u64;
    for (id, source) in sources {
        for byte in id
            .as_str()
            .bytes()
            .chain([0])
            .chain(source.bytes())
            .chain([0xff])
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

pub(crate) fn stable_image_hash(source_hash: u64, settings: &serde_json::Value) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V2");
    feed_hash(&mut hash, &source_hash.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_ABI_VERSION.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_COMPILER_VERSION.to_be_bytes());
    hash_json(&mut hash, settings);
    hash
}

fn feed_hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn hash_json(hash: &mut u64, value: &serde_json::Value) {
'''
replacement = '''pub(crate) fn stable_source_hash<'a>(
    sources: impl Iterator<Item = (&'a SourceId, &'a str)>,
) -> String {
    // Runtime Image authority is security-sensitive. Canonicalize the source
    // set and bind the complete SourceId + source bytes with SHA-256 instead of
    // the former 64-bit non-cryptographic FNV identity.
    let mut sources = sources.collect::<Vec<_>>();
    sources.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));

    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_SOURCE_SET_V1");
    feed_hash(&mut hash, &(sources.len() as u64).to_be_bytes());
    for (id, source) in sources {
        feed_hash(&mut hash, id.as_str().as_bytes());
        feed_hash(&mut hash, source.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(crate) fn stable_image_hash(source_hash: &str, settings: &serde_json::Value) -> String {
    let mut hash = Sha256::new();
    feed_hash(&mut hash, b"RBE_RUNTIME_IMAGE_V3");
    feed_hash(&mut hash, source_hash.as_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_ABI_VERSION.to_be_bytes());
    feed_hash(&mut hash, &ROUTE_WASM_COMPILER_VERSION.to_be_bytes());
    hash_json(&mut hash, settings);
    hex::encode(hash.finalize())
}

fn feed_hash(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn hash_json(hash: &mut Sha256, value: &serde_json::Value) {
'''
rep(runtime, start, replacement)

rep(runtime,
'''    fn image_hash_is_sensitive_to_settings_and_key_order_is_stable() {
        let source_hash = 42;
        let first = serde_json::json!({"runtimeEnv": {"A": 1, "B": true}});
        let reordered = serde_json::json!({"runtimeEnv": {"B": true, "A": 1}});
        let changed = serde_json::json!({"runtimeEnv": {"A": 2, "B": true}});
        assert_eq!(
            stable_image_hash(source_hash, &first),
            stable_image_hash(source_hash, &reordered)
        );
        assert_ne!(
            stable_image_hash(source_hash, &first),
            stable_image_hash(source_hash, &changed)
        );
    }
''',
'''    fn image_hash_is_sensitive_to_settings_and_key_order_is_stable() {
        let source_hash = "2a".repeat(32);
        let first = serde_json::json!({"runtimeEnv": {"A": 1, "B": true}});
        let reordered = serde_json::json!({"runtimeEnv": {"B": true, "A": 1}});
        let changed = serde_json::json!({"runtimeEnv": {"A": 2, "B": true}});
        let first_hash = stable_image_hash(&source_hash, &first);
        assert_eq!(first_hash, stable_image_hash(&source_hash, &reordered));
        assert_ne!(first_hash, stable_image_hash(&source_hash, &changed));
        assert_eq!(first_hash.len(), 64);
        assert!(first_hash.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
''')
rep(runtime,
'''        assert_eq!(first, same);
        assert_ne!(first, other);
    }
}''',
'''        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}''')

relc = 'engine/crates/route-engine/src/relc.rs'
rep(relc,
'''    let image_hash = stable_image_hash(source_hash, settings_json);
    Ok(RuntimeImage {
        image_id: format!("rbe-{image_hash:016x}"),
        source_hash,
''',
'''    let image_hash = stable_image_hash(&source_hash, settings_json);
    Ok(RuntimeImage {
        // The Controller Capability Firewall uses this exact SHA-256 identity.
        image_id: image_hash,
        source_hash,
''')

boot = 'engine/crates/backend/src/runtime_image_boot.rs'
rep(boot,
'''        image = %image.image_id,
        source_hash = format_args!("{:016x}", image.source_hash),
''',
'''        image = %image.image_id,
        source_hash = %image.source_hash,
''')
