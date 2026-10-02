use std::env;
use std::path::PathBuf;

use route_engine::{prepare_service_oid_cache, OidCache};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut project_root = env::current_dir()?;
    let mut materialize = false;
    let mut record = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--materialize" => materialize = true,
            "--record" => {
                let value = args.next().ok_or("--record requires an OID")?;
                record = Some(value.parse::<u16>()?);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run -p route-engine --example oid_inspect -- [PROJECT_ROOT] [--materialize] [--record OID]"
                );
                return Ok(());
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option {value:?}").into());
            }
            value => project_root = PathBuf::from(value),
        }
    }

    if materialize {
        let report = prepare_service_oid_cache(&project_root)?;
        println!(
            "materialized core OIDs: target={} generation={} total={} written={} reused={}",
            report.target.label(),
            report.index_generation,
            report.total,
            report.written,
            report.reused
        );
    }

    let cache = OidCache::open_or_rebuild(&project_root)?;
    print!("{}", cache.inspect()?);

    if let Some(oid) = record {
        let record = cache.read_record(oid)?;
        println!("OID {oid}");
        println!("  name: {}", record.name);
        println!("  kind: {:?}", record.kind);
        println!("  flags: 0x{:08x}", record.flags);
        println!("  target: {}", record.target.label());
        println!("  entry offset: {}", record.entry_offset);
        println!("  alignment: {}", record.alignment);
        println!("  required OIDs: {:?}", record.required_oids);
        println!("  relocations: {}", record.relocations.len());
        println!("  diagnostics: {}", record.diagnostics.len());
        for diagnostic in &record.diagnostics {
            println!("    {}", diagnostic.render_control());
        }
        println!("  machine code bytes: {}", record.machine_code.len());
        println!(
            "  machine code: {}",
            record
                .machine_code
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    Ok(())
}
