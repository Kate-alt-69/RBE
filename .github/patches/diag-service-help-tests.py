from pathlib import Path

path = Path('engine/crates/backend/src/service_main.rs')
text = path.read_text()
marker = 'fn fallback_service_diagnostics_link_to_error_code_book()'
if marker in text:
    raise SystemExit('Service help regression test already exists')

text += r'''

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn fallback_service_diagnostics_link_to_error_code_book() {
        let error = anyhow::anyhow!("synthetic startup failure");

        let mother = render_fatal("SVC5099", "Service Mother failed to start.", &error);
        assert!(mother.contains("doc/error-codes/service.md#svc5099"));

        let worker = render_fatal("SVC5199", "Service worker failed to start.", &error);
        assert!(worker.contains("doc/error-codes/service.md#svc5199"));
    }
}
'''
path.write_text(text)
