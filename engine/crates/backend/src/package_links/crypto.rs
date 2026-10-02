use anyhow::{bail, Context};
use core_lib::{
    LibraryCapabilityGrant, LibraryHostCall, LibraryHostCallReply, LibrarySessionBinding,
    MAX_LIBRARY_PAYLOAD_BYTES,
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

pub const CAPABILITY: &str = "crypto";
const MAX_RANDOM_BYTES: usize = 4096;
const MAX_DATA_BYTES: usize = 512 * 1024;
const MAX_KEY_BYTES: usize = 64 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RandomRequest {
    bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DataRequest {
    data_hex: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HmacRequest {
    key_hex: String,
    data_hex: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EqualRequest {
    left_hex: String,
    right_hex: String,
}

pub fn grant() -> anyhow::Result<LibraryCapabilityGrant> {
    LibraryCapabilityGrant::new(
        CAPABILITY,
        CAPABILITY,
        [
            "random".to_string(),
            "sha256".to_string(),
            "hmac_sha256".to_string(),
            "constant_time_eq".to_string(),
        ],
        MAX_LIBRARY_PAYLOAD_BYTES,
        MAX_LIBRARY_PAYLOAD_BYTES,
    )
    .context("build verified package crypto capability grant")
}

pub fn dispatch_authorized_call(
    binding: &LibrarySessionBinding,
    call: &LibraryHostCall,
) -> anyhow::Result<LibraryHostCallReply> {
    let grant = binding
        .authorize_host_call(call)
        .context("authorize package crypto call against accepted Library Host session")?;
    if call.capability != CAPABILITY || call.target != CAPABILITY {
        bail!("package crypto call does not match admitted crypto authority");
    }

    let payload = match call.operation.as_str() {
        "random" => random(&call.payload)?,
        "sha256" => sha256(&call.payload)?,
        "hmac_sha256" => hmac_sha256(&call.payload)?,
        "constant_time_eq" => constant_time_eq(&call.payload)?,
        other => bail!("unsupported package crypto operation {other:?}"),
    };
    if payload.len() > grant.max_response_bytes {
        bail!(
            "package crypto response exceeded admitted capability limit: limit={}, observed={}",
            grant.max_response_bytes,
            payload.len()
        );
    }
    LibraryHostCallReply::success(call.call_id, payload)
        .context("encode successful package crypto host-call reply")
}

fn random(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: RandomRequest =
        serde_json::from_slice(payload).context("decode package crypto random request")?;
    if request.bytes == 0 || request.bytes > MAX_RANDOM_BYTES {
        bail!("package crypto random byte count must be in 1..={MAX_RANDOM_BYTES}");
    }
    let mut data = vec![0u8; request.bytes];
    rand::rngs::OsRng.fill_bytes(&mut data);
    serde_json::to_vec(&json!({ "data_hex": hex::encode(data) }))
        .context("encode package crypto random response")
}

fn sha256(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: DataRequest =
        serde_json::from_slice(payload).context("decode package crypto SHA-256 request")?;
    let data = decode_hex_bounded(&request.data_hex, MAX_DATA_BYTES, "data_hex")?;
    let digest = Sha256::digest(data);
    serde_json::to_vec(&json!({ "digest_hex": hex::encode(digest) }))
        .context("encode package crypto SHA-256 response")
}

fn hmac_sha256(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: HmacRequest =
        serde_json::from_slice(payload).context("decode package crypto HMAC-SHA256 request")?;
    let key = decode_hex_bounded(&request.key_hex, MAX_KEY_BYTES, "key_hex")?;
    if key.is_empty() {
        bail!("package crypto HMAC key cannot be empty");
    }
    let data = decode_hex_bounded(&request.data_hex, MAX_DATA_BYTES, "data_hex")?;
    let mut mac = HmacSha256::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("package crypto HMAC key is invalid"))?;
    mac.update(&data);
    let digest = mac.finalize().into_bytes();
    serde_json::to_vec(&json!({ "digest_hex": hex::encode(digest) }))
        .context("encode package crypto HMAC-SHA256 response")
}

fn constant_time_eq(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let request: EqualRequest = serde_json::from_slice(payload)
        .context("decode package crypto constant-time compare request")?;
    let left = decode_hex_bounded(&request.left_hex, MAX_DATA_BYTES, "left_hex")?;
    let right = decode_hex_bounded(&request.right_hex, MAX_DATA_BYTES, "right_hex")?;

    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    serde_json::to_vec(&json!({ "equal": difference == 0 }))
        .context("encode package crypto constant-time compare response")
}

fn decode_hex_bounded(value: &str, max_bytes: usize, label: &str) -> anyhow::Result<Vec<u8>> {
    if value.len() > max_bytes.saturating_mul(2) {
        bail!("package crypto {label} exceeds maximum size");
    }
    hex::decode(value)
        .with_context(|| format!("package crypto {label} must contain valid hexadecimal bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_compare_checks_length_and_content() {
        let equal = constant_time_eq(br#"{"left_hex":"aabb","right_hex":"aabb"}"#).unwrap();
        let equal: serde_json::Value = serde_json::from_slice(&equal).unwrap();
        assert_eq!(equal["equal"], true);

        let different = constant_time_eq(br#"{"left_hex":"aabb","right_hex":"aabc"}"#).unwrap();
        let different: serde_json::Value = serde_json::from_slice(&different).unwrap();
        assert_eq!(different["equal"], false);

        let different_length =
            constant_time_eq(br#"{"left_hex":"aa","right_hex":"aabb"}"#).unwrap();
        let different_length: serde_json::Value =
            serde_json::from_slice(&different_length).unwrap();
        assert_eq!(different_length["equal"], false);
    }
}
