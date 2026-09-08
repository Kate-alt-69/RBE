from pathlib import Path

manager = Path("engine/crates/service-runtime/src/manager.rs")
source = manager.read_text()

impl_anchor = '''impl ServiceManager {
    pub fn remote(address: SocketAddr, auth: String) -> anyhow::Result<Self> {'''
impl_replacement = '''impl ServiceManager {
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    pub fn remote(address: SocketAddr, auth: String) -> anyhow::Result<Self> {'''
if impl_anchor not in source:
    raise SystemExit("ServiceManager impl anchor changed")
source = source.replace(impl_anchor, impl_replacement, 1)

shutdown_anchor = '''    pub async fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::Release);'''
shutdown_replacement = '''    pub async fn shutdown_all(&self) {
        self.begin_shutdown();'''
if shutdown_anchor not in source:
    raise SystemExit("ServiceManager shutdown anchor changed")
source = source.replace(shutdown_anchor, shutdown_replacement, 1)

manager_test_anchor = '''    #[tokio::test]
    async fn service_shutdown_drain_waits_for_active_call() {'''
manager_test = '''    #[tokio::test]
    async fn begin_shutdown_closes_service_call_admission() {
        let manager = ServiceManager::default();
        manager.begin_shutdown();
        let error = manager.call("missing", "run", Vec::new()).await.unwrap_err();
        assert!(matches!(error, ServiceCallError::Unavailable { .. }));
    }

'''
if manager_test_anchor not in source:
    raise SystemExit("ServiceManager test anchor changed")
source = source.replace(manager_test_anchor, manager_test + manager_test_anchor, 1)
manager.write_text(source)

mother = Path("engine/crates/service-runtime/src/mother.rs")
source = mother.read_text()
shutdown_arm = '''        ServiceMotherRequest::Shutdown { .. } => {
            // Acknowledge the authenticated control request before draining
            // child services. The listener loop owns the actual shutdown so
            // service teardown runs exactly once and cannot consume the
            // client's bounded response window.
            let _ = shutdown_tx.send(true);'''
shutdown_arm_replacement = '''        ServiceMotherRequest::Shutdown { .. } => {
            // Close admission immediately, then acknowledge the authenticated
            // control request before draining child services. The listener loop
            // owns the actual shutdown so teardown runs exactly once and cannot
            // consume the client's bounded response window.
            manager.begin_shutdown();
            let _ = shutdown_tx.send(true);'''
if shutdown_arm not in source:
    raise SystemExit("Service Mother shutdown arm anchor changed")
source = source.replace(shutdown_arm, shutdown_arm_replacement, 1)

old_test_setup = '''        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let server_token = Arc::<str>::from(token.clone());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, ServiceManager::default(), server_token, shutdown_tx)
                .await
                .unwrap();
        });'''
new_test_setup = '''        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let server_token = Arc::<str>::from(token.clone());
        let manager = ServiceManager::default();
        let server_manager = manager.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, server_manager, server_token, shutdown_tx)
                .await
                .unwrap();
        });'''
if old_test_setup not in source:
    raise SystemExit("Service Mother shutdown test setup changed")
source = source.replace(old_test_setup, new_test_setup, 1)

old_test_tail = '''        shutdown_rx.changed().await.unwrap();
        assert!(*shutdown_rx.borrow());
        server.await.unwrap();
    }'''
new_test_tail = '''        shutdown_rx.changed().await.unwrap();
        assert!(*shutdown_rx.borrow());
        let error = manager.call("missing", "run", Vec::new()).await.unwrap_err();
        assert!(matches!(error, ServiceCallError::Unavailable { .. }));
        server.await.unwrap();
    }'''
if old_test_tail not in source:
    raise SystemExit("Service Mother shutdown test tail changed")
source = source.replace(old_test_tail, new_test_tail, 1)
mother.write_text(source)
