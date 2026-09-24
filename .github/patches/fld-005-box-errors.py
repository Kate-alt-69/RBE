from pathlib import Path

path = Path("engine/crates/route-engine/src/discovery.rs")
text = path.read_text()

replacements = [
    (
        ") -> Result<Vec<u8>, Response> {\n",
        ") -> Result<Vec<u8>, Box<Response>> {\n",
    ),
    (
        '''        request_error(\n            StatusCode::INTERNAL_SERVER_ERROR,\n            "native route input could not be encoded",\n        )\n''',
        '''        Box::new(request_error(\n            StatusCode::INTERNAL_SERVER_ERROR,\n            "native route input could not be encoded",\n        ))\n''',
    ),
    (
        '''        return Err(request_error(\n            StatusCode::PAYLOAD_TOO_LARGE,\n            too_large_message,\n        ));\n''',
        '''        return Err(Box::new(request_error(\n            StatusCode::PAYLOAD_TOO_LARGE,\n            too_large_message,\n        )));\n''',
    ),
    (
        "Err(response) => return response,",
        "Err(response) => return *response,",
    ),
]

for old, new in replacements:
    count = text.count(old)
    expected = 4 if old == "Err(response) => return response," else 1
    if count != expected:
        raise SystemExit(
            f"unexpected FLD-005 box-error anchor count: expected {expected}, got {count}: {old[:80]!r}"
        )
    text = text.replace(old, new)

path.write_text(text)
