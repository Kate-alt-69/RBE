from pathlib import Path

path = Path("engine/crates/service-runtime/src/mother.rs")
source = path.read_text()

old = '''        ServiceMotherRequest::Snapshot { .. } => ServiceMotherResponse::Snapshots {
            services: manager.snapshot().await,
        },
        ServiceMotherRequest::Shutdown { .. } => {
            manager.shutdown_all().await;
            let _ = shutdown_tx.send(true);
            ServiceMotherResponse::Ok
        }
    };
    write_response(&mut write, &response).await?;
    Ok(())
}'''
new = '''        ServiceMotherRequest::Snapshot { .. } => ServiceMotherResponse::Snapshots {
            services: manager.snapshot().await,
        },
        ServiceMotherRequest::Shutdown { .. } => {
            // Acknowledge the authenticated control request before draining
            // child services. The listener loop owns the actual shutdown so
            // service teardown runs exactly once and cannot consume the
            // client's bounded response window.
            let _ = shutdown_tx.send(true);
            write_response(&mut write, &ServiceMotherResponse::Ok).await?;
            return Ok(());
        }
    };
    write_response(&mut write, &response).await?;
    Ok(())
}'''
if old not in source:
    raise SystemExit("Service Mother shutdown handler anchor changed")
source = source.replace(old, new, 1)

anchor = '''    #[test]
    fn mother_tokens_are_256_bit_hex() {'''
test = '''    #[tokio::test]
    async fn shutdown_request_is_acknowledged_and_signals_listener() {
        let token = "a".repeat(64);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let server_token = Arc::<str>::from(token.clone());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, ServiceManager::default(), server_token, shutdown_tx)
                .await
                .unwrap();
        });

        let response = mother_rpc(
            address,
            ServiceMotherRequest::Shutdown { token },
        )
        .await
        .unwrap();
        assert!(matches!(response, ServiceMotherResponse::Ok));
        shutdown_rx.changed().await.unwrap();
        assert!(*shutdown_rx.borrow());
        server.await.unwrap();
    }

'''
if anchor not in source:
    raise SystemExit("Service Mother test anchor changed")
source = source.replace(anchor, test + anchor, 1)
path.write_text(source)
