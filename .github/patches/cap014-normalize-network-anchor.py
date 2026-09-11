from pathlib import Path

path = Path("engine/crates/core/src/network_broker.rs")
text = path.read_text(encoding="utf-8")
old = '''pub async fn call_public_http(operation: &str, args: &[Value]) -> Result<Value, PublicHttpError> {
    let call = parse_http_call(operation, args)?;'''
new = '''pub async fn call_public_http(
    operation: &str,
    args: &[Value],
) -> Result<Value, PublicHttpError> {
    let call = parse_http_call(operation, args)?;'''
if text.count(old) != 1:
    raise SystemExit(f"network broker signature anchor changed: {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
