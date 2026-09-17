from pathlib import Path

path = Path('.github/patches/storage-rel-write-v2.py')
text = path.read_text(encoding='utf-8')
old = '''    let level_valid = level
        .as_u64()
        .is_some_and(|value| (1..=3).contains(&value));
    if !level_valid {
        return Err("native storage.write level[] must be 1, 2, or 3".into());
    }
    if !encoding.is_string() {
'''
new = '''    let level = level
        .as_f64()
        .filter(|value| value.fract() == 0.0 && (1.0..=3.0).contains(value))
        .ok_or_else(|| "native storage.write level[] must be 1, 2, or 3".to_string())?;
    let level = serde_json::Value::Number(serde_json::Number::from(level as u64));
    if !encoding.is_string() {
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f'level integer normalization anchor count={count}')
path.write_text(text.replace(old, new, 1), encoding='utf-8')
