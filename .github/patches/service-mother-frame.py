from pathlib import Path

path = Path("crates/service-runtime/src/mother.rs")
source = path.read_text()
source = source.replace(
    "use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};",
    "use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};",
    1,
)
old = '''    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 || bytes > 4 * 1024 * 1024 {
        anyhow::bail!("Service Mother request is empty or too large");
    }
    let request: ServiceMotherRequest = serde_json::from_str(line.trim())?;'''
new = '''    let (read, mut write) = stream.into_split();
    let line = read_bounded_line(read, 4 * 1024 * 1024, "request").await?;
    let request: ServiceMotherRequest = serde_json::from_str(line.trim())?;'''
if old not in source:
    raise SystemExit("mother request read anchor missing")
source = source.replace(old, new, 1)
old = '''    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 || bytes > 8 * 1024 * 1024 {
        anyhow::bail!("Service Mother response is empty or too large");
    }
    Ok(serde_json::from_str(line.trim())?)
}'''
new = '''    let line = read_bounded_line(read, 8 * 1024 * 1024, "response").await?;
    Ok(serde_json::from_str(line.trim())?)
}

async fn read_bounded_line<R>(reader: R, max_bytes: usize, label: &str) -> anyhow::Result<String>
where
    R: AsyncRead + Unpin,
{
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = BufReader::new(reader).take(limit);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        anyhow::bail!("Service Mother {label} is empty");
    }
    if bytes > max_bytes {
        anyhow::bail!("Service Mother {label} exceeded {max_bytes} bytes");
    }
    if !line.ends_with('\\n') {
        anyhow::bail!("Service Mother {label} is not newline terminated");
    }
    Ok(line)
}'''
if old not in source:
    raise SystemExit("mother response read anchor missing")
source = source.replace(old, new, 1)
insert = r'''

    #[tokio::test]
    async fn bounded_frame_reader_rejects_oversized_and_unterminated_frames() {
        assert!(read_bounded_line(&b"abcd\n"[..], 3, "test").await.is_err());
        assert!(read_bounded_line(&b"abc"[..], 3, "test").await.is_err());
        assert_eq!(
            read_bounded_line(&b"abc\n"[..], 4, "test").await.unwrap(),
            "abc\n"
        );
    }
'''
end = source.rfind("\n}")
if end < 0:
    raise SystemExit("mother tests tail missing")
source = source[:end] + insert + source[end:]
path.write_text(source)
