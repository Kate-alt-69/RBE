from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


# IPC v7: artifact registration carries immutable execution identity and the
# response confirms the exact binding Controller persisted.
ipc = Path("container-runtime/crates/ipc-protocol/src/lib.rs")
replace_once(
    ipc,
    "pub const PROTOCOL_VERSION: u16 = 6;",
    "pub const PROTOCOL_VERSION: u16 = 7;",
    "protocol v7",
)
replace_once(
    ipc,
    '''pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}''',
    '''pub struct RegisterArtifactRequest {
    pub request_id: String,
    pub auth_token: String,
    pub runtime_image: String,
    pub source_id: String,
    pub capability_abi: u16,
    pub artifact_hash: String,
    pub wasm: Vec<u8>,
}''',
    "artifact request identity",
)
replace_once(
    ipc,
    '''    ArtifactRegistered {
        request_id: String,
        artifact_hash: String,
        already_present: bool,
    },''',
    '''    ArtifactRegistered {
        request_id: String,
        runtime_image: String,
        source_id: String,
        capability_abi: u16,
        artifact_hash: String,
        already_present: bool,
        already_bound: bool,
    },''',
    "artifact response binding",
)
replace_once(
    ipc,
    '''        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: "artifact-1".into(),
            auth_token: "secret".into(),
            artifact_hash: "ab".repeat(32),
            wasm: vec![0, 97, 115, 109],
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        assert!(matches!(decoded, Request::RegisterArtifact(_)));''',
    '''        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: "artifact-1".into(),
            auth_token: "secret".into(),
            runtime_image: "ab".repeat(32),
            source_id: "route:api/me".into(),
            capability_abi: CAPABILITY_ABI_VERSION,
            artifact_hash: "cd".repeat(32),
            wasm: vec![0, 97, 115, 109],
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded = decode_request(&read_frame(&mut bytes.as_slice()).unwrap()).unwrap();
        let Request::RegisterArtifact(decoded) = decoded else {
            panic!("expected artifact registration request");
        };
        assert_eq!(decoded.runtime_image, "ab".repeat(32));
        assert_eq!(decoded.source_id, "route:api/me");
        assert_eq!(decoded.capability_abi, CAPABILITY_ABI_VERSION);
        assert_eq!(decoded.artifact_hash, "cd".repeat(32));''',
    "artifact protocol test identity",
)

# Capability Broker owns the immutable RuntimeImage/SourceId -> artifact
# authority relation. Artifact bytes remain deduplicated in Runtime cache.
broker = Path("container-runtime/crates/container-runtime-core/src/control_plane.rs")
replace_once(
    broker,
    '''struct ManifestKey {
    runtime_image: String,
    source_id: String,
    environment: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityManifest {''',
    '''struct ManifestKey {
    runtime_image: String,
    source_id: String,
    environment: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ArtifactBindingKey {
    runtime_image: String,
    source_id: String,
    capability_abi: u16,
    artifact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityManifest {''',
    "artifact binding key",
)
replace_once(
    broker,
    '''pub struct CapabilityBroker {
    debug_enabled: bool,
    manifests: RwLock<HashMap<ManifestKey, CapabilityManifest>>,
}''',
    '''pub struct CapabilityBroker {
    debug_enabled: bool,
    manifests: RwLock<HashMap<ManifestKey, CapabilityManifest>>,
    artifact_bindings: RwLock<HashSet<ArtifactBindingKey>>,
}''',
    "artifact binding table",
)
replace_once(
    broker,
    '''        Self {
            debug_enabled,
            manifests: RwLock::new(HashMap::new()),
        }''',
    '''        Self {
            debug_enabled,
            manifests: RwLock::new(HashMap::new()),
            artifact_bindings: RwLock::new(HashSet::new()),
        }''',
    "artifact binding table init",
)
replace_once(
    broker,
    '''    /// Verify that an execution identity is explicitly bound to a manifest
    /// for the Controller's current Environment generation. Empty manifests are
    /// valid and intentionally distinguish "no host capabilities" from
    /// "identity was never registered".
    pub fn authorize_execution(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        generation: u64,
        capability_abi: u16,
    ) -> Result<(), CapabilityError> {
        validate_runtime_image(runtime_image)?;
        validate_source_id(source_id)?;
        validate_environment(environment)?;
        if capability_abi != CAPABILITY_ABI_VERSION {
            return Err(CapabilityError {
                code: "CAPABILITY_ABI_UNSUPPORTED",
                message: format!(
                    "capability ABI {capability_abi} is unsupported; controller supports {CAPABILITY_ABI_VERSION}"
                ),
            });
        }
        let key = ManifestKey {
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            environment: environment.to_string(),
            generation,
        };
        if self
            .manifests
            .read()
            .expect("capability manifest table poisoned")
            .contains_key(&key)
        {
            Ok(())
        } else {
            Err(CapabilityError {
                code: "CAPABILITY_MANIFEST_UNKNOWN",
                message: "execution has no exact capability manifest for this Runtime Image/SourceId/Environment generation".into(),
            })
        }
    }
''',
    '''    /// Bind immutable WASM identity to the exact Runtime Image source that
    /// registered it. The same artifact bytes may be intentionally shared by
    /// multiple sources/images, but each authority edge must be explicit.
    pub fn register_artifact_binding(
        &self,
        runtime_image: &str,
        source_id: &str,
        capability_abi: u16,
        artifact_hash: &str,
    ) -> Result<bool, CapabilityError> {
        validate_runtime_image(runtime_image)?;
        validate_source_id(source_id)?;
        validate_artifact_hash(artifact_hash)?;
        if capability_abi != CAPABILITY_ABI_VERSION {
            return Err(CapabilityError {
                code: "CAPABILITY_ABI_UNSUPPORTED",
                message: format!(
                    "capability ABI {capability_abi} is unsupported; controller supports {CAPABILITY_ABI_VERSION}"
                ),
            });
        }
        let key = ArtifactBindingKey {
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            capability_abi,
            artifact_hash: artifact_hash.to_string(),
        };
        let mut bindings = self
            .artifact_bindings
            .write()
            .expect("artifact binding table poisoned");
        Ok(!bindings.insert(key))
    }

    /// Verify that execution has both an exact capability manifest for the live
    /// Environment generation and an explicit artifact provenance binding.
    pub fn authorize_execution(
        &self,
        runtime_image: &str,
        source_id: &str,
        environment: &str,
        generation: u64,
        capability_abi: u16,
        artifact_hash: &str,
    ) -> Result<(), CapabilityError> {
        validate_runtime_image(runtime_image)?;
        validate_source_id(source_id)?;
        validate_environment(environment)?;
        validate_artifact_hash(artifact_hash)?;
        if capability_abi != CAPABILITY_ABI_VERSION {
            return Err(CapabilityError {
                code: "CAPABILITY_ABI_UNSUPPORTED",
                message: format!(
                    "capability ABI {capability_abi} is unsupported; controller supports {CAPABILITY_ABI_VERSION}"
                ),
            });
        }
        let manifest_key = ManifestKey {
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            environment: environment.to_string(),
            generation,
        };
        if !self
            .manifests
            .read()
            .expect("capability manifest table poisoned")
            .contains_key(&manifest_key)
        {
            return Err(CapabilityError {
                code: "CAPABILITY_MANIFEST_UNKNOWN",
                message: "execution has no exact capability manifest for this Runtime Image/SourceId/Environment generation".into(),
            });
        }

        let artifact_key = ArtifactBindingKey {
            runtime_image: runtime_image.to_string(),
            source_id: source_id.to_string(),
            capability_abi,
            artifact_hash: artifact_hash.to_string(),
        };
        if self
            .artifact_bindings
            .read()
            .expect("artifact binding table poisoned")
            .contains(&artifact_key)
        {
            Ok(())
        } else {
            Err(CapabilityError {
                code: "ARTIFACT_BINDING_UNKNOWN",
                message: "execution artifact is not bound to this Runtime Image/SourceId/capability ABI".into(),
            })
        }
    }
''',
    "artifact provenance authorization",
)
replace_once(
    broker,
    '''    pub fn manifest_count(&self) -> usize {
        self.manifests
            .read()
            .expect("capability manifest table poisoned")
            .len()
    }
}''',
    '''    pub fn manifest_count(&self) -> usize {
        self.manifests
            .read()
            .expect("capability manifest table poisoned")
            .len()
    }

    pub fn artifact_binding_count(&self) -> usize {
        self.artifact_bindings
            .read()
            .expect("artifact binding table poisoned")
            .len()
    }
}''',
    "artifact binding count",
)
replace_once(
    broker,
    '''fn validate_source_id(value: &str) -> Result<(), CapabilityError> {
    if value.is_empty()
        || value.len() > MAX_SOURCE_ID_BYTES
        || value.contains('\\0')
        || value.chars().any(char::is_control)
    {
        return Err(invalid_identity(
            "source_id",
            "is empty, too long, or invalid",
        ));
    }
    Ok(())
}

fn validate_environment''',
    '''fn validate_source_id(value: &str) -> Result<(), CapabilityError> {
    if value.is_empty()
        || value.len() > MAX_SOURCE_ID_BYTES
        || value.contains('\\0')
        || value.chars().any(char::is_control)
    {
        return Err(invalid_identity(
            "source_id",
            "is empty, too long, or invalid",
        ));
    }
    Ok(())
}

fn validate_artifact_hash(value: &str) -> Result<(), CapabilityError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_identity(
            "artifact_hash",
            "must be lowercase 64-character SHA-256",
        ));
    }
    Ok(())
}

fn validate_environment''',
    "artifact hash validation",
)
# Replace the execution-binding test as one unit so all new arguments and the
# fail-closed mismatched-artifact assertion stay coherent.
replace_once(
    broker,
    '''    #[test]
    fn execution_binding_requires_exact_controller_generation_and_abi() {
        let broker = CapabilityBroker::new(false);
        broker.register_manifest(&request(Vec::new()), 4).unwrap();
        broker
            .authorize_execution(
                &"ab".repeat(32),
                "route:api/me",
                "general-1",
                4,
                CAPABILITY_ABI_VERSION,
            )
            .unwrap();
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    5,
                    CAPABILITY_ABI_VERSION,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_MANIFEST_UNKNOWN"
        );
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    4,
                    CAPABILITY_ABI_VERSION + 1,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_ABI_UNSUPPORTED"
        );
    }''',
    '''    #[test]
    fn execution_binding_requires_exact_generation_abi_and_artifact() {
        let broker = CapabilityBroker::new(false);
        let artifact = "cd".repeat(32);
        broker.register_manifest(&request(Vec::new()), 4).unwrap();
        assert!(!broker
            .register_artifact_binding(
                &"ab".repeat(32),
                "route:api/me",
                CAPABILITY_ABI_VERSION,
                &artifact,
            )
            .unwrap());
        assert!(broker
            .register_artifact_binding(
                &"ab".repeat(32),
                "route:api/me",
                CAPABILITY_ABI_VERSION,
                &artifact,
            )
            .unwrap());
        assert_eq!(broker.artifact_binding_count(), 1);
        broker
            .authorize_execution(
                &"ab".repeat(32),
                "route:api/me",
                "general-1",
                4,
                CAPABILITY_ABI_VERSION,
                &artifact,
            )
            .unwrap();
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    5,
                    CAPABILITY_ABI_VERSION,
                    &artifact,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_MANIFEST_UNKNOWN"
        );
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    4,
                    CAPABILITY_ABI_VERSION + 1,
                    &artifact,
                )
                .unwrap_err()
                .code,
            "CAPABILITY_ABI_UNSUPPORTED"
        );
        assert_eq!(
            broker
                .authorize_execution(
                    &"ab".repeat(32),
                    "route:api/me",
                    "general-1",
                    4,
                    CAPABILITY_ABI_VERSION,
                    &"ef".repeat(32),
                )
                .unwrap_err()
                .code,
            "ARTIFACT_BINDING_UNKNOWN"
        );
    }''',
    "execution artifact binding test",
)

# Controller persists the binding only after bytes pass the existing SHA-256
# registration check. Execute checks the exact binding in the same broker that
# owns capability manifests.
controller = Path("container-runtime/crates/container-bin/src/main.rs")
replace_once(
    controller,
    '''            } else {
                match runtime.register_artifact(&request.artifact_hash, request.wasm) {
                    Ok(already_present) => Response::ArtifactRegistered {
                        request_id: request.request_id,
                        artifact_hash: request.artifact_hash,
                        already_present,
                    },
                    Err(message) => Response::Error {
                        request_id: Some(request.request_id),
                        code: "ARTIFACT_HASH_MISMATCH".into(),
                        message,
                    },
                }
            }
        }''',
    '''            } else {
                match runtime.register_artifact(&request.artifact_hash, request.wasm) {
                    Ok(already_present) => match capability_broker.register_artifact_binding(
                        &request.runtime_image,
                        &request.source_id,
                        request.capability_abi,
                        &request.artifact_hash,
                    ) {
                        Ok(already_bound) => Response::ArtifactRegistered {
                            request_id: request.request_id,
                            runtime_image: request.runtime_image,
                            source_id: request.source_id,
                            capability_abi: request.capability_abi,
                            artifact_hash: request.artifact_hash,
                            already_present,
                            already_bound,
                        },
                        Err(error) => Response::Error {
                            request_id: Some(request.request_id),
                            code: error.code.into(),
                            message: error.message,
                        },
                    },
                    Err(message) => Response::Error {
                        request_id: Some(request.request_id),
                        code: "ARTIFACT_HASH_MISMATCH".into(),
                        message,
                    },
                }
            }
        }''',
    "Controller artifact binding registration",
)
replace_once(
    controller,
    '''                    generation,
                    request.capability_abi,
                ) {''',
    '''                    generation,
                    request.capability_abi,
                    &request.artifact_hash,
                ) {''',
    "Controller artifact binding authorization",
)

# Backend registration now sends the same immutable identity that will be used
# for manifest registration/execution and verifies Controller echoed it.
client = Path("engine/crates/core/src/container_client.rs")
replace_once(
    client,
    '''    pub async fn register_artifact(
        &self,
        artifact_hash: &str,
        wasm: Vec<u8>,
    ) -> anyhow::Result<bool> {''',
    '''    pub async fn register_artifact(
        &self,
        identity: ContainerExecutionIdentity<'_>,
        artifact_hash: &str,
        wasm: Vec<u8>,
    ) -> anyhow::Result<bool> {''',
    "client artifact identity argument",
)
replace_once(
    client,
    '''        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            artifact_hash: artifact_hash.to_string(),
            wasm,
        });
        match call(endpoint, request, Duration::from_secs(10)).await? {
            Response::ArtifactRegistered {
                already_present, ..
            } => Ok(already_present),''',
    '''        let request = Request::RegisterArtifact(RegisterArtifactRequest {
            request_id: next_request_id(),
            auth_token: endpoint.token.clone(),
            runtime_image: identity.runtime_image.to_string(),
            source_id: identity.source_id.to_string(),
            capability_abi: CAPABILITY_ABI_VERSION,
            artifact_hash: artifact_hash.to_string(),
            wasm,
        });
        match call(endpoint, request, Duration::from_secs(10)).await? {
            Response::ArtifactRegistered {
                runtime_image,
                source_id,
                capability_abi,
                already_present,
                ..
            } if runtime_image == identity.runtime_image
                && source_id == identity.source_id
                && capability_abi == CAPABILITY_ABI_VERSION => Ok(already_present),
            Response::ArtifactRegistered { .. } => {
                anyhow::bail!("Container returned a mismatched artifact provenance binding")
            }''',
    "client artifact response identity",
)
replace_once(
    client,
    '''        self.register_artifact(request.artifact_hash, request.wasm)
            .await?;''',
    '''        self.register_artifact(request.identity, request.artifact_hash, request.wasm)
            .await?;''',
    "authorized artifact identity registration",
)

# Document the now-enforced artifact provenance edge.
doc = Path("doc/runtime-image.md")
replace_once(
    doc,
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image.

Replacing an Environment generation also invalidates grants tied to the previous generation, so image identity and process-generation identity participate together in the capability boundary.''',
    '''The Container capability broker validates the Runtime Image ID as a lowercase 64-character SHA-256 string. A manifest registered for one image cannot silently authorize a different linked image.

Native WASM artifact registration is also bound to the exact `Runtime Image ID + SourceId + capability ABI`. Artifact bytes remain deduplicated by SHA-256 in the Container cache, but cache presence alone is never execution authority: Execute must present an artifact hash explicitly registered for that Runtime Image source.

Replacing an Environment generation also invalidates grants tied to the previous generation, so image identity and process-generation identity participate together in the capability boundary.''',
    "artifact provenance documentation",
)
