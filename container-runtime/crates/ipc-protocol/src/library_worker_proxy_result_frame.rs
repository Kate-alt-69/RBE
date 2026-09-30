use std::io::{self, Read, Write};

use crate::{read_frame, write_frame, LibraryWorkerProxyResult};

pub fn write_library_worker_proxy_result<W: Write>(
    writer: &mut W,
    result: &LibraryWorkerProxyResult,
) -> io::Result<()> {
    result
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    write_frame(writer, result)
}

pub fn read_library_worker_proxy_result<R: Read>(
    reader: &mut R,
) -> io::Result<LibraryWorkerProxyResult> {
    let body = read_frame(reader)?;
    let result: LibraryWorkerProxyResult = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    result
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn result_round_trips_through_container_framing() {
        let expected = LibraryWorkerProxyResult::Completed {
            exit_code: 0,
            stdout: b"hello".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
            output_limit_exceeded: false,
            cgroup_enforced: true,
            wall_time_ms: 5,
        };
        let mut bytes = Vec::new();
        write_library_worker_proxy_result(&mut bytes, &expected).unwrap();
        let actual = read_library_worker_proxy_result(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(actual, expected);
    }
}
