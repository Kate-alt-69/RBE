from pathlib import Path

path = Path("crates/service-runtime/src/lib.rs")
source = path.read_text()
old = "mod manager;\npub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};\n"
new = "mod manager;\nmod mother;\npub use manager::{ServiceCallError, ServiceManager, ServiceRuntimeState, ServiceSnapshot};\npub use mother::{new_service_mother_token, run_service_mother, ServiceMotherReady};\n"
if old not in source:
    raise SystemExit("service-runtime module anchor missing")
path.write_text(source.replace(old, new, 1))

path = Path("crates/service-runtime/src/manager.rs")
source = path.read_text()
source = source.replace("use serde::Serialize;", "use serde::{Deserialize, Serialize};", 1)
anchor = "use super::{\n    RestartPolicy, ServiceCatalog, ServiceFile, ServiceMode, ServiceReady, ServiceRequest,\n    ServiceResponse,\n};"
if anchor not in source:
    raise SystemExit("manager import anchor missing")
source = source.replace(anchor, anchor + "\nuse crate::mother::ServiceMotherClient;", 1)
old = "#[derive(Clone, Default)]\npub struct ServiceManager {\n    services: Arc<AsyncRwLock<HashMap<String, Arc<Mutex<Managed>>>>>,\n    shutting_down: Arc<AtomicBool>,\n}"
new = "#[derive(Clone, Default)]\npub struct ServiceManager {\n    services: Arc<AsyncRwLock<HashMap<String, Arc<Mutex<Managed>>>>>,\n    shutting_down: Arc<AtomicBool>,\n    mother: Option<ServiceMotherClient>,\n}"
if old not in source:
    raise SystemExit("manager struct anchor missing")
source = source.replace(old, new, 1)
source = source.replace("#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum ServiceRuntimeState", "#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum ServiceRuntimeState", 1)
source = source.replace("#[derive(Debug, Clone, Serialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct ServiceSnapshot", "#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(rename_all = \"camelCase\")]\npub struct ServiceSnapshot", 1)
anchor = "impl ServiceManager {\n    pub async fn spawn_all"
if anchor not in source:
    raise SystemExit("manager impl anchor missing")
source = source.replace(anchor, '''impl ServiceManager {
    pub fn remote(address: SocketAddr, auth: String) -> anyhow::Result<Self> {
        if !address.ip().is_loopback() {
            anyhow::bail!("Service Mother endpoint must be loopback");
        }
        if auth.len() != 64 || !auth.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("Service Mother authentication value must be 256-bit hexadecimal");
        }
        Ok(Self {
            mother: Some(ServiceMotherClient::new(address, auth)),
            ..Self::default()
        })
    }

    pub async fn spawn_all''', 1)
anchor = '''        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ServiceCallError::Unavailable {
                service: service_name.to_string(),
            });
        }

        let handle = self
'''
replacement = '''        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ServiceCallError::Unavailable {
                service: service_name.to_string(),
            });
        }
        if let Some(mother) = &self.mother {
            return match operation {
                ServiceOperation::Call { function, args } => mother.call(service_name, &function, args).await,
                ServiceOperation::Event { event } => mother.event(service_name, event).await,
            };
        }

        let handle = self
'''
if anchor not in source:
    raise SystemExit("invoke anchor missing")
source = source.replace(anchor, replacement, 1)
anchor = "    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {\n        let handles = self\n"
replacement = '''    pub async fn snapshot(&self) -> Vec<ServiceSnapshot> {
        if let Some(mother) = &self.mother {
            return match mother.snapshot().await {
                Ok(services) => services,
                Err(error) => vec![ServiceSnapshot {
                    name: "service-mother".into(),
                    title: "RBE Service Mother".into(),
                    pid: None,
                    state: ServiceRuntimeState::Unknown,
                    mode: ServiceMode::Resident,
                    restart: RestartPolicy::OnFailure,
                    restart_attempts: 0,
                    idle_timeout_ms: 0,
                    ready: false,
                    health_checked: true,
                    health: None,
                    health_error: Some(error.to_string()),
                }],
            };
        }
        let handles = self
'''
if anchor not in source:
    raise SystemExit("snapshot anchor missing")
source = source.replace(anchor, replacement, 1)
anchor = "    pub async fn shutdown_all(&self) {\n        self.shutting_down.store(true, Ordering::Release);\n        let handles = self\n"
replacement = '''    pub async fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::Release);
        if let Some(mother) = &self.mother {
            if let Err(error) = mother.shutdown().await {
                tracing::warn!(error = %error, "Service Mother shutdown RPC failed");
            }
            return;
        }
        let handles = self
'''
if anchor not in source:
    raise SystemExit("shutdown anchor missing")
source = source.replace(anchor, replacement, 1)
path.write_text(source)

path = Path("crates/service-runtime/src/mother.rs")
source = path.read_text()
source = source.replace('ServiceMotherResponse::Error { code, message } if code == "SVCM404" =>', 'ServiceMotherResponse::Error { code, message: _ } if code == "SVCM404" =>')
source = source.replace('ServiceMotherResponse::Error { code, message } if code == "SVCM503" =>', 'ServiceMotherResponse::Error { code, message: _ } if code == "SVCM503" =>')
path.write_text(source)
