from pathlib import Path

path = Path("container-runtime/crates/container-runtime-core/src/control_plane.rs")
text = path.read_text(encoding="utf-8")
old = '''    fn debug_controller_still_denies_debug_and_host_file_to_secure_profile() {
        let broker = CapabilityBroker::new(true);
        for kind in [CapabilityKind::Debug, CapabilityKind::HostFile] {
            let mut general_grant = service_grant();'''
new = '''    fn debug_controller_still_denies_debug_and_host_file_to_secure_profile() {
        for kind in [CapabilityKind::Debug, CapabilityKind::HostFile] {
            // Each capability kind gets a fresh broker because manifests are
            // intentionally immutable for one exact identity/generation key.
            let broker = CapabilityBroker::new(true);
            let mut general_grant = service_grant();'''
if text.count(old) != 1:
    raise SystemExit(f"CTR-006 test isolation anchor count={text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
