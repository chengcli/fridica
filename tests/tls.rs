use fridica::exec::tls;

// A separate process prevents another test's TLS client from selecting a provider first.
#[test]
fn crypto_provider_is_initialized_in_a_fresh_process() {
    for scenario in ["select", "preserve"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "provider_fixture", "--nocapture"])
            .env("FRIDICA_TLS_FIXTURE", scenario)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn provider_fixture() {
    let Ok(scenario) = std::env::var("FRIDICA_TLS_FIXTURE") else {
        return;
    };
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    if scenario == "preserve" {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .unwrap();
    }
    let previous = rustls::crypto::CryptoProvider::get_default().cloned();
    let threads: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(tls::initialize))
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    if let Some(previous) = previous {
        assert!(std::sync::Arc::ptr_eq(
            &previous,
            rustls::crypto::CryptoProvider::get_default().unwrap()
        ));
    }
    // Exercise the WSS path that formerly panicked on a Tokio worker thread.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });
    let client = tokio::spawn(async move {
        tokio_tungstenite::connect_async(format!("wss://{address}/socket")).await
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap();
    assert!(result.is_ok(), "TLS initialization panicked");
    assert!(
        result.unwrap().is_err(),
        "untrusted peer must not establish TLS"
    );
    peer.await.unwrap();
}
