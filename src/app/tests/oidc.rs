use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn refresh_key_uses_a_refreshed_oidc_token() {
    const CHILD: &str = "SOFKA_TEST_OIDC_REFRESH";
    if std::env::var_os(CHILD).is_none() {
        // Set trust in a child process so other tests keep their own trust settings.
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "app::tests::oidc::refresh_key_uses_a_refreshed_oidc_token",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(
                "SSL_CERT_FILE",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/ca.pem"),
            )
            .env_remove("SSL_CERT_DIR")
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .expect("OIDC child test timeout")
            .expect("run OIDC child test");
        assert!(
            output.status.success(),
            "OIDC child test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![
                CertificateDer::from_pem_slice(include_bytes!(
                    "../../../tests/fixtures/tls/server.pem"
                ))
                .unwrap(),
            ],
            PrivateKeyDer::from_pem_slice(include_bytes!("../../../tests/fixtures/tls/server.key"))
                .unwrap(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let token_endpoint = format!("{issuer}/token");
    let refreshed_token = oidc_test_token(4_070_908_800);
    let returned_token = refreshed_token.clone();
    let server = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        // Each client must discover the issuer and exchange its expired token.
        for _ in 0..2 {
            for path in ["/.well-known/openid-configuration", "/token"] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await.unwrap());
                    assert!(bytes.len() < 16384);
                }
                let request = String::from_utf8(bytes).unwrap();
                let headers: HashMap<_, _> = request
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
                    .collect();
                let body = if path == "/token" {
                    assert_eq!(request.lines().next(), Some("POST /token HTTP/1.1"));
                    assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
                    assert_eq!(
                        headers["authorization"],
                        format!("Basic {}", STANDARD.encode("test-client:test-secret"))
                    );
                    let length: usize = headers["content-length"].parse().unwrap();
                    assert!(length < 4096);
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    let params: HashMap<_, _> = form_urlencoded::parse(&body).collect();
                    assert_eq!(params["grant_type"], "refresh_token");
                    assert_eq!(params["refresh_token"], "test-refresh-token");
                    json!({
                        "id_token": returned_token,
                        "access_token": "different-access-token",
                        "refresh_token": "new-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })
                } else {
                    assert_eq!(
                        request.lines().next(),
                        Some("GET /.well-known/openid-configuration HTTP/1.1")
                    );
                    json!({"token_endpoint": token_endpoint})
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        }
    });

    for configured_ca in [false, true] {
        let provider = serde_json::from_value(json!({
            "name": "oidc",
            "config": {
                "id-token": oidc_test_token(1),
                "idp-issuer-url": issuer,
                "client-id": "test-client",
                "client-secret": "test-secret",
                "refresh-token": "test-refresh-token"
            }
        }))
        .unwrap();
        read_pods_with_tls_and_auth(configured_ca, Some(provider), Some(&refreshed_token)).await;
    }
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("mock issuer timeout")
        .expect("mock issuer failed");
}
