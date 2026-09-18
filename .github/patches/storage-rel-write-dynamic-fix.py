from pathlib import Path

path = Path("engine/crates/route-engine/src/wasm_compiler.rs")
text = path.read_text(encoding="utf-8")

old = '''    let take = |values: &mut BTreeMap<String, &Expr>, name: &str| {
        values
            .remove(name)
            .ok_or_else(|| format!("dynamic storage.write is missing {name}[]"))
    };
    let mut values = descriptors;
    let encoding_expr = take(&mut values, "encode")?;
    let data_expr = take(&mut values, "data")?;
    let path_expr = take(&mut values, "write")?;
    let level_expr = take(&mut values, "level")?;
'''
new = '''    let mut values = descriptors;
    let encoding_expr = values
        .remove("encode")
        .ok_or_else(|| "dynamic storage.write is missing encode[]".to_string())?;
    let data_expr = values
        .remove("data")
        .ok_or_else(|| "dynamic storage.write is missing data[]".to_string())?;
    let path_expr = values
        .remove("write")
        .ok_or_else(|| "dynamic storage.write is missing write[]".to_string())?;
    let level_expr = values
        .remove("level")
        .ok_or_else(|| "dynamic storage.write is missing level[]".to_string())?;
'''
if text.count(old) != 1:
    raise SystemExit(f"dynamic descriptor lifetime fix anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = "    let instructions = run.instructions();\n"
new = "    let mut instructions = run.instructions();\n"
if text.count(old) != 1:
    raise SystemExit(f"dynamic instruction mutability fix anchor count={text.count(old)}")
text = text.replace(old, new, 1)

path.write_text(text, encoding="utf-8")
