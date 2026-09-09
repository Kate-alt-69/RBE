from pathlib import Path
import re

manager = Path('engine/crates/service-runtime/src/manager.rs')
text = manager.read_text(encoding='utf-8')
text, count = re.subn(
    r'\n    fn running\(file: ServiceFile, process: ServiceProcess\) -> Self \{.*?\n    \}\n(?=\n    fn wakeable)',
    '',
    text,
    count=1,
    flags=re.S,
)
if count != 1:
    raise SystemExit('failed to remove obsolete Managed::running constructor')
manager.write_text(text, encoding='utf-8')

mother = Path('engine/crates/backend/src/service_mother.rs')
text = mother.read_text(encoding='utf-8')
old = '''            Ok(())
        }
        None => server_task.await??,
'''
new = '''        }
        None => server_task.await??,
'''
if old not in text:
    raise SystemExit('failed to find Service Mother match result fixup')
mother.write_text(text.replace(old, new, 1), encoding='utf-8')

lib = Path('engine/crates/route-engine/src/lib.rs')
text = lib.read_text(encoding='utf-8')
old = '''    #[test]
    fn rejects_service_to_service_imports() {
        let error = parse_service_source(
            r#"
            :import[service:other]
            :service[name = current]
            export function run() { return true; }
        "#,
        )
        .expect_err("service-to-service import should fail");
        assert!(error.message.contains("service-to-service"));
    }
'''
new = '''    #[test]
    fn parses_service_to_service_imports_for_fabric() {
        let file = parse_service_source(
            r#"
            :import[service:other]
            :service[name = current]
            export function run() { return true; }
        "#,
        )
        .expect("service-to-service import should be accepted for Service Fabric");
        assert!(matches!(
            file.imports.as_slice(),
            [ImportTarget::Service(name)] if name == "other"
        ));
    }
'''
if old not in text:
    raise SystemExit('failed to find obsolete service-to-service rejection test')
lib.write_text(text.replace(old, new, 1), encoding='utf-8')
