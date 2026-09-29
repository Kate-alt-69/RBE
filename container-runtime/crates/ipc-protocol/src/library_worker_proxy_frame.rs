use std::io::{self, Read, Write};

use crate::{read_frame, write_frame, LibraryWorkerProxyBootstrap};

/// Write one validated Backend -> Container Library Worker Proxy bootstrap.
///
/// Validation happens before any bytes are emitted, so a caller cannot partially
/// write a bootstrap that widens environment/network/shell authority and then
/// discover the contract error afterward.
pub fn write_library_worker_proxy_bootstrap<W: Write>(
    writer: &mut W,
    bootstrap: &LibraryWorkerProxyBootstrap,
) -> io::Result<()> {
    bootstrap
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    write_frame(writer, bootstrap)
}

/// Read exactly one length-prefixed Library Worker Proxy bootstrap and validate
/// the decoded contract before returning it to Container execution code.
///
/// The generic Container frame limit applies first; serde shape validation and
/// the stricter Library Worker Proxy invariants are then enforced again on the
/// receiving side.
pub fn read_library_worker_proxy_bootstrap<R: Read>(
    reader: &mut R,
) -> io::Result<LibraryWorkerProxyBootstrap> {
    let body = read_frame(reader)?;
    let bootstrap: LibraryWorkerProxyBootstrap = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bootstrap
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(bootstrap)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use super::*;
    use crate::{
        LibraryWorkerProxySourceFile, LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
    };

    fn absolute_program() -> String {
        #[cfg(windows)]
        {
            r"C:\managed\bun.exe".to_string()
        }
        #[cfg(not(windows))]
        {
            "/managed/bun".to_string()
        }
    }

    fn absolute_root() -> String {
        #[cfg(windows)]
        {
            r"C:\worker".to_string()
        }
        #[cfg(not(windows))]
        {
            "/worker".to_string()
        }
    }

    fn absolute_entrypoint() -> String {
        #[cfg(windows)]
        {
            r"C:\worker\worker.js".to_string()
        }
        #[cfg(not(windows))]
        {
            "/worker/worker.js".to_string()
        }
    }

    fn bootstrap() -> LibraryWorkerProxyBootstrap {
        LibraryWorkerProxyBootstrap {
            protocol: LIBRARY_WORKER_PROXY_PROTOCOL_VERSION,
            program: absolute_program(),
            program_sha256: "a".repeat(64),
            args: vec![absolute_entrypoint()],
            working_directory: absolute_root(),
            source_files: vec![LibraryWorkerProxySourceFile {
                path: "worker.js".into(),
                size: 7,
                sha256: "b".repeat(64),
            }],
            clear_environment: true,
            environment: BTreeMap::new(),
            direct_network_allowed: false,
            use_shell: false,
            startup_timeout_seconds: 30,
        }
    }

    #[test]
    fn valid_proxy_bootstrap_round_trips_through_container_framing() {
        let expected = bootstrap();
        let mut bytes = Vec::new();
        write_library_worker_proxy_bootstrap(&mut bytes, &expected).unwrap();

        let decoded =
            read_library_worker_proxy_bootstrap(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn invalid_proxy_bootstrap_is_rejected_before_any_bytes_are_written() {
        let mut invalid = bootstrap();
        invalid.direct_network_allowed = true;
        let mut bytes = Vec::new();

        let error = write_library_worker_proxy_bootstrap(&mut bytes, &invalid).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bytes.is_empty());
    }

    #[test]
    fn receiver_revalidates_bootstrap_even_if_sender_bypassed_helper() {
        let mut invalid = bootstrap();
        invalid.use_shell = true;
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &invalid).unwrap();

        let error =
            read_library_worker_proxy_bootstrap(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_bootstrap_fields_are_rejected_during_decode() {
        let valid = bootstrap();
        let mut value = serde_json::to_value(valid).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();

        let error =
            read_library_worker_proxy_bootstrap(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
