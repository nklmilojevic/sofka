use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_headers(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Option<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(stream.read_u8().await.ok()?);
        assert!(bytes.len() < 16384);
    }
    String::from_utf8(bytes).ok()
}

#[tokio::test]
async fn proxy_exclusions_apply_on_startup_and_context_switch() {
    const CHILD: &str = "SOFKA_TEST_PROXY_CONNECTION";
    if let Ok(phase) = std::env::var(CHILD) {
        // SAFETY: this child process runs only this test, and `main` does the
        // same before its runtime starts.
        unsafe { crate::k8s::sanitize_no_proxy_env() };
        let (mut app, mut rx) = test_app();
        if phase == "startup" {
            app.cluster = Cluster::connect(false, false)
                .await
                .expect("connect at startup");
            app.kind = app.cluster.resolve("pods");
            app.kind_plural = "pods".into();
            app.handle_key(press(KeyCode::Char('r'))).unwrap();
        } else {
            type_resource_query(&mut app, "pods --context target");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let msg = rx.recv().await.expect("context result");
                    let switched = matches!(&msg, Msg::ContextSwitched { .. });
                    if let Msg::ContextSwitched { result, .. } = &msg {
                        assert!(
                            result.is_ok(),
                            "context connection failed: {:?}",
                            result.as_ref().err()
                        );
                    }
                    app.handle_msg(msg);
                    if switched {
                        break;
                    }
                }
            })
            .await
            .expect("context switch timeout");
            assert_eq!(app.cluster.context, "target");
        }
        sync_selector_view(&mut app, &mut rx).await;
        assert_eq!(row_names(&app), ["direct-pod"]);
        app.handle_key(press(KeyCode::Char('q'))).unwrap();
        return;
    }

    let certificate = include_bytes!("../../../tests/fixtures/tls/proxy-ca.pem");
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(certificate).unwrap()],
            PrivateKeyDer::from_pem_slice(include_bytes!("../../../tests/fixtures/tls/server.key"))
                .unwrap(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let api = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            connections.spawn(async move {
                let Ok(mut stream) = acceptor.accept(socket).await else { return };
                let Some(request) = read_headers(&mut stream).await else { return };
                let path = request.split_whitespace().nth(1).unwrap();
                let body = match path {
                    "/apis" => json!({"kind":"APIGroupList", "apiVersion":"v1", "groups":[]}),
                    "/api" => json!({"kind":"APIVersions", "apiVersion":"v1", "versions":["v1"], "serverAddressByClientCIDRs":[]}),
                    "/api/v1" => json!({"kind":"APIResourceList", "apiVersion":"v1", "groupVersion":"v1", "resources":[{"name":"pods", "singularName":"pod", "namespaced":true, "kind":"Pod", "verbs":["get","list","watch"]}]}),
                    "/version" => json!({"major":"1", "minor":"35", "gitVersion":"v1.35.0", "gitCommit":"", "gitTreeState":"clean", "buildDate":"", "goVersion":"", "compiler":"", "platform":"linux/amd64"}),
                    _ if path.contains("/pods?") => {
                        let body = concat!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                            "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"direct-pod\",\"namespace\":\"default\",\"resourceVersion\":\"1\"}}}\n",
                            "{\"type\":\"BOOKMARK\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"resourceVersion\":\"1\",\"annotations\":{\"k8s.io/initial-events-end\":\"true\"}}}}\n",
                        );
                        let _ = stream.write_all(body.as_bytes()).await;
                        std::future::pending::<()>().await;
                        return;
                    }
                    _ => {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                }.to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let proxy_connections = Arc::new(AtomicUsize::new(0));
    let seen = proxy_connections.clone();
    let proxy = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            seen.fetch_add(1, Ordering::SeqCst);
            connections.spawn(async move {
                let Some(request) = read_headers(&mut socket).await else {
                    return;
                };
                assert!(
                    request.starts_with(&format!("CONNECT {address} ")),
                    "{request}"
                );
                let mut upstream = tokio::net::TcpStream::connect(address).await.unwrap();
                socket
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            });
        }
    });
    let directory = std::env::temp_dir().join(format!(
        "sofka-no-proxy-{}-{}",
        std::process::id(),
        address.port()
    ));
    std::fs::create_dir(&directory).unwrap();
    let config_path = directory.join("kubeconfig");
    // Excludes the proxy itself but not the API server, which listens on
    // another port of the same address.
    let proxy_only = proxy_url.trim_start_matches("http://");
    for (upper, lower, explicit, use_proxy) in [
        (Some("127.0.0.1"), None, false, false),
        (None, Some("127.0.0.1"), false, false),
        (Some(""), Some("127.0.0.1"), false, false),
        (Some(" \t\n"), Some("127.0.0.1"), false, false),
        (Some(" \t"), Some("\n "), false, true),
        (Some("unmatched.invalid"), Some("127.0.0.1"), false, true),
        (Some("*"), None, false, false),
        (Some("127.0.0.0/8"), None, false, false),
        (Some("127.0.0.1,"), None, false, false),
        (Some("<local>,,127.0.0.1"), None, false, false),
        (Some("<local>,"), Some("127.0.0.1"), false, true),
        (Some(proxy_only), None, false, true),
        (Some(proxy_only), None, true, true),
        (None, None, false, true),
        (Some("*"), None, true, true),
    ] {
        for phase in ["startup", "context"] {
            let mut cluster =
                json!({"server":format!("https://{address}"), "tls-server-name":"localhost"});
            // Cover both the standard TLS client and the configured CA transport.
            if phase == "startup" {
                cluster["insecure-skip-tls-verify"] = true.into();
            } else {
                cluster["certificate-authority-data"] = STANDARD.encode(certificate).into();
            }
            let mut other = cluster.clone();
            if explicit {
                cluster["proxy-url"] = proxy_url.clone().into();
            }
            if !explicit {
                other["proxy-url"] = proxy_url.clone().into();
            }
            let document = json!({
                "apiVersion":"v1", "kind":"Config", "current-context":"startup",
                "clusters":[{"name":"selected", "cluster":cluster}, {"name":"other", "cluster":other}],
                "contexts":[
                    {"name":"startup", "context":{"cluster":if phase == "startup" {"selected"} else {"other"}}},
                    {"name":"target", "context":{"cluster":"selected"}}
                ]
            });
            std::fs::write(&config_path, document.to_string()).unwrap();
            let before = proxy_connections.load(Ordering::SeqCst);
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "app::tests::proxy::proxy_exclusions_apply_on_startup_and_context_switch",
                    "--nocapture",
                ])
                .env(CHILD, phase)
                .env("KUBECONFIG", &config_path)
                .env_remove("HTTPS_PROXY")
                .env_remove("https_proxy")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .env_remove("HTTP_PROXY")
                .env_remove("http_proxy")
                .env_remove("ALL_PROXY")
                .env_remove("all_proxy")
                .env_remove("KUBE_RS_DEBUG_IMPERSONATE_USER")
                .env_remove("KUBE_RS_DEBUG_IMPERSONATE_GROUP")
                .env_remove("KUBE_RS_DEBUG_OVERRIDE_URL")
                .env(
                    if phase == "startup" {
                        "HTTPS_PROXY"
                    } else {
                        "https_proxy"
                    },
                    &proxy_url,
                )
                .kill_on_drop(true);
            if let Some(value) = upper {
                command.env("NO_PROXY", value);
            }
            if let Some(value) = lower {
                command.env("no_proxy", value);
            }
            let output = tokio::time::timeout(Duration::from_secs(15), command.output())
                .await
                .expect("child timeout")
                .unwrap();
            assert!(
                output.status.success(),
                "{phase}, {upper:?}, {lower:?}, explicit={explicit}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                proxy_connections.load(Ordering::SeqCst) > before,
                use_proxy,
                "{phase}, {upper:?}, {lower:?}, explicit={explicit}"
            );
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
    proxy.abort();
    api.abort();
}
