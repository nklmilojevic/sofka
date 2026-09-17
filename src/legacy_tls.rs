//! Kubernetes TLS compatibility and the shared HTTP transport.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{Request, Response, Uri};
use hyper::rt::{Read, Write};
use hyper_rustls::{FixedServerNameResolver, HttpsConnector, HttpsConnectorBuilder};
use hyper_timeout::TimeoutConnector;
use hyper_util::{
    client::legacy::connect::{
        Connection, HttpConnector,
        proxy::{SocksV5, Tunnel},
    },
    rt::TokioExecutor,
};
use kube::{
    Config,
    client::{Body, ClientBuilder, ConfigExt, DynBody, middleware::AuthLayer, retry::RetryPolicy},
};
use rustls::{
    ClientConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    sign::{CertifiedKey, SingleCertAndKey},
};
use secrecy::{ExposeSecret, SecretSlice};
use tower::{BoxError, Service, ServiceBuilder, ServiceExt, retry::RetryLayer, util::BoxService};
use tower_http::{ServiceExt as _, decompression::DecompressionLayer, trace::TraceLayer};
use x509_parser::prelude::*;

type Builder = ClientBuilder<BoxService<Request<Body>, Response<Box<DynBody>>, BoxError>>;

pub(crate) fn client_builder(
    config: Config,
    allow_v1: bool,
    no_tls_resumption: bool,
) -> Result<Builder> {
    let roots = crate::server_tls::configured_roots(&config);
    if roots.is_none() && !no_tls_resumption {
        return match ClientBuilder::try_from(config.clone()) {
            Ok(builder) => Ok(builder),
            Err(error) => {
                let tls = v1_config(&config, allow_v1, error)?;
                let auth = config.auth_layer()?;
                connect(config, tls, auth)
            }
        };
    }
    // Resolve auth before the TLS identity, as kube-rs does.
    let auth = config.auth_layer()?;
    let mut tls = match config.rustls_client_config() {
        Ok(tls) => tls,
        Err(error) => v1_config(&config, allow_v1, error)?,
    };
    if let Some(roots) = roots {
        crate::server_tls::install(&mut tls, roots)?;
    }
    if no_tls_resumption {
        tls.resumption = rustls::client::Resumption::disabled();
    }
    // Kube-rs exposes exec expiry only through its standard client builder.
    // Retain that metadata when the opt-in path needs a custom transport.
    let expiration = if config.auth_info.exec.is_some() {
        *ClientBuilder::try_from(config.clone())?
            .build()
            .valid_until()
    } else {
        None
    };
    Ok(connect(config, tls, auth)?.with_valid_until(expiration))
}

fn v1_config(config: &Config, allow_v1: bool, original_error: kube::Error) -> Result<ClientConfig> {
    if !matches!(
        &original_error,
        kube::Error::RustlsTls(kube::client::RustlsTlsError::InvalidPrivateKey(
            rustls::Error::InvalidCertificate(_)
        ))
    ) {
        return Err(original_error.into());
    }
    // Exec certificates take precedence over static credentials in kube-rs.
    if config.auth_info.exec.is_some() {
        return Err(original_error).context(
            "unsupported certificate version; --allow-v1-client-cert supports static kubeconfig client certificates only",
        );
    }
    let Some(chain) = certificate_chain(config)? else {
        return Err(original_error.into());
    };
    let cert = parse_certificate(&chain[0])?;
    if cert.version() != X509Version::V1 {
        return Err(original_error.into());
    }
    ensure!(
        allow_v1,
        "client certificate is X.509 v1; use a v3 certificate or pass --allow-v1-client-cert for this run (see docs/debugging.md#x509-v1-client-certificates)"
    );
    legacy_config(config, chain)
}

fn connect(config: Config, tls: ClientConfig, auth: Option<AuthLayer>) -> Result<Builder> {
    let mut connector = HttpConnector::new();
    connector.enforce_http(false);
    match config.proxy_url.as_ref() {
        None => transport(connector, config, tls, auth),
        Some(proxy) if proxy.scheme_str() == Some("socks5") => {
            transport(SocksV5::new(proxy.clone(), connector), config, tls, auth)
        }
        Some(proxy) if proxy.scheme_str() == Some("http") => {
            let connector = proxy_auth(proxy, Tunnel::new(proxy.clone(), connector))?;
            transport(connector, config, tls, auth)
        }
        Some(proxy) if proxy.scheme_str() == Some("https") => {
            // Use the configured trust settings for the proxy, without sending
            // the cluster client certificate or overriding the proxy hostname.
            let mut proxy_config = without_identity(&config);
            proxy_config.tls_server_name = None;
            let connector = proxy_config.rustls_https_connector_with_connector(connector)?;
            let connector = proxy_auth(proxy, Tunnel::new(proxy.clone(), connector))?;
            transport(connector, config, tls, auth)
        }
        Some(proxy) => bail!("unsupported proxy protocol: {:?}", proxy.scheme_str()),
    }
}

fn load(data: Option<&str>, path: Option<&str>) -> Result<Option<Vec<u8>>> {
    match (data, path) {
        (Some(data), _) => Ok(Some(
            STANDARD
                .decode(data)
                .context("decoding client credential data")?,
        )),
        (_, Some(path)) => Ok(Some(
            std::fs::read(path).context("reading client credential file")?,
        )),
        _ => Ok(None),
    }
}

fn certificate_chain(config: &Config) -> Result<Option<Vec<CertificateDer<'static>>>> {
    let auth = &config.auth_info;
    let Some(pem) = load(
        auth.client_certificate_data.as_deref(),
        auth.client_certificate.as_deref(),
    )?
    else {
        return Ok(None);
    };
    let chain = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .context("reading client certificate PEM")?;
    ensure!(!chain.is_empty(), "client certificate chain is empty");
    Ok(Some(chain))
}

fn parse_certificate(der: &[u8]) -> Result<X509Certificate<'_>> {
    let (rest, cert) = X509Certificate::from_der(der)
        .map_err(|_| anyhow::anyhow!("invalid client certificate DER"))?;
    ensure!(rest.is_empty(), "client certificate has trailing DER data");
    Ok(cert)
}

fn without_identity(config: &Config) -> Config {
    let mut config = config.clone();
    config.auth_info.client_certificate = None;
    config.auth_info.client_certificate_data = None;
    config.auth_info.client_key = None;
    config.auth_info.client_key_data = None;
    config
}

fn legacy_config(config: &Config, chain: Vec<CertificateDer<'static>>) -> Result<ClientConfig> {
    let cert = parse_certificate(&chain[0])?;
    ensure!(
        cert.version() == X509Version::V1,
        "compatibility requires an X.509 v1 client certificate"
    );
    ensure!(
        cert.extensions().is_empty() && cert.issuer_uid.is_none() && cert.subject_uid.is_none(),
        "invalid X.509 v1 client certificate: extensions or unique IDs are present"
    );
    for intermediate in &chain[1..] {
        parse_certificate(intermediate)?;
    }
    let auth = &config.auth_info;
    let pem = SecretSlice::from(
        load(
            auth.client_key_data.as_ref().map(|s| s.expose_secret()),
            auth.client_key.as_deref(),
        )?
        .context("client certificate has no private key")?,
    );
    let key = PrivateKeyDer::from_pem_slice(pem.expose_secret())
        .context("reading client private key PEM")?;
    // Retain kube-rs server verification, native roots and CA file reloads.
    let mut tls = without_identity(config).rustls_client_config()?;
    let key = tls
        .crypto_provider()
        .key_provider
        .load_private_key(key)
        .context("loading client private key")?;
    let public_key = key
        .public_key()
        .context("cannot check the client private key's public key")?;
    ensure!(
        public_key.as_ref() == cert.public_key().raw,
        "client certificate does not match its private key"
    );
    tls.client_auth_cert_resolver = Arc::new(SingleCertAndKey::from(CertifiedKey::new(chain, key)));
    Ok(tls)
}

fn proxy_auth<C>(proxy: &Uri, connector: Tunnel<C>) -> Result<Tunnel<C>> {
    if let Some(authority) = proxy.authority()
        && let Some((userinfo, _)) = authority.as_str().split_once('@')
    {
        let header = format!("Basic {}", STANDARD.encode(userinfo)).parse()?;
        return Ok(connector.with_auth(header));
    }
    Ok(connector)
}

fn https<H>(connector: H, config: &Config, tls: ClientConfig) -> Result<HttpsConnector<H>> {
    let mut builder = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http();
    if let Some(name) = &config.tls_server_name {
        builder = builder.with_server_name_resolver(FixedServerNameResolver::new(
            name.clone().try_into().context("invalid TLS server name")?,
        ));
    }
    Ok(builder.enable_http1().wrap_connector(connector))
}

fn transport<H>(
    connector: H,
    config: Config,
    tls: ClientConfig,
    auth: Option<AuthLayer>,
) -> Result<Builder>
where
    H: 'static + Clone + Send + Sync + Service<Uri>,
    H::Response: 'static + Connection + Read + Write + Send + Unpin,
    H::Future: 'static + Send,
    H::Error: 'static + Send + Sync + std::error::Error,
{
    let mut connector = TimeoutConnector::new(https(connector, &config, tls)?);
    connector.set_connect_timeout(config.connect_timeout);
    connector.set_read_timeout(config.read_timeout);
    connector.set_write_timeout(config.write_timeout);
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
    let service = ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .layer(
            DecompressionLayer::new()
                .no_br()
                .no_deflate()
                .no_zstd()
                .gzip(!config.disable_compression),
        )
        .option_layer(
            config
                .default_retry
                .then_some(RetryLayer::new(RetryPolicy::server_retry())),
        )
        .option_layer(auth)
        .layer(config.extra_headers_layer()?)
        .layer(TraceLayer::new_for_http())
        .map_err(BoxError::from)
        .service(client)
        .map_response_body(|body| {
            Box::new(http_body_util::BodyExt::map_err(body, BoxError::from)) as Box<DynBody>
        })
        .boxed();
    Ok(ClientBuilder::new(service, config.default_namespace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{
        DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
        client::danger::HandshakeSignatureValid,
        pki_types::{SubjectPublicKeyInfoDer, UnixTime},
        server::danger::{ClientCertVerified, ClientCertVerifier},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    const PROXY: &[u8] = include_bytes!("../tests/fixtures/tls/proxy-ca.pem");
    const CA: &[u8] = include_bytes!("../tests/fixtures/tls/ca.pem");
    const CLIENT: &[u8] = include_bytes!("../tests/fixtures/tls/client-v1.pem");
    const CLIENT_V3: &[u8] = include_bytes!("../tests/fixtures/tls/client.pem");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/tls/client.key");
    const SERVER: &[u8] = include_bytes!("../tests/fixtures/tls/server.pem");
    const EXPIRED: &[u8] = include_bytes!("../tests/fixtures/tls/server-expired.pem");
    const SERVER_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/server.key");
    const CLIENT_P521: &[u8] = include_bytes!("../tests/fixtures/tls/client-p521.pem");
    const CLIENT_P521_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/client-p521.key");

    fn config() -> Config {
        let mut config = Config::new("https://127.0.0.1:6443/prefix".parse().unwrap());
        config.root_cert = Some(vec![CertificateDer::from_pem_slice(CA).unwrap().to_vec()]);
        config.tls_server_name = Some("localhost".into());
        config.auth_info.client_certificate_data = Some(STANDARD.encode(CLIENT));
        config.auth_info.client_key_data = Some(STANDARD.encode(KEY).into());
        config.default_namespace = "test-ns".into();
        config.default_retry = false;
        config
    }

    #[tokio::test]
    async fn v1_requires_consent_even_when_server_verification_is_disabled() {
        for insecure in [false, true] {
            let mut config = config();
            config.accept_invalid_certs = insecure;
            let error = client_builder(config, false, false)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("client certificate is X.509 v1"), "{error}");
            assert!(error.contains("--allow-v1-client-cert"), "{error}");
        }
        assert!(client_builder(config(), true, false).is_ok());
    }

    #[tokio::test]
    async fn consent_does_not_allow_a_mismatched_or_invalid_key() {
        for key in [SERVER_KEY, b"invalid key"] {
            let mut config = config();
            config.auth_info.client_key_data = Some(STANDARD.encode(key).into());
            assert!(client_builder(config, true, false).is_err());
        }
        let mut config = config();
        config.auth_info.client_key_data = Some(STANDARD.encode(SERVER_KEY).into());
        let error = client_builder(config, true, false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("does not match its private key"), "{error}");
    }

    #[tokio::test]
    async fn v3_uses_the_normal_path_with_or_without_consent() {
        for allow in [false, true] {
            let mut config = config();
            config.auth_info.client_certificate_data = Some(STANDARD.encode(CLIENT_V3));
            assert!(client_builder(config.clone(), allow, false).is_ok());
            config.auth_info.client_key_data = Some(STANDARD.encode(SERVER_KEY).into());
            assert!(client_builder(config, allow, false).is_err());
        }
    }

    #[tokio::test]
    async fn p521_client_keys_require_a_matching_certificate_with_any_tls_flags() {
        for insecure in [false, true] {
            for allow_v1 in [false, true] {
                let mut config = config();
                config.accept_invalid_certs = insecure;
                config.auth_info.client_certificate_data = Some(STANDARD.encode(CLIENT_P521));
                config.auth_info.client_key_data = Some(STANDARD.encode(CLIENT_P521_KEY).into());
                assert!(client_builder(config.clone(), allow_v1, false).is_ok());
                config.auth_info.client_key_data = Some(STANDARD.encode(KEY).into());
                assert!(client_builder(config, allow_v1, false).is_err());
            }
        }
    }

    #[tokio::test]
    async fn consent_does_not_allow_v2_or_malformed_certificates() {
        let mut v2 = CertificateDer::from_pem_slice(CLIENT_V3).unwrap().to_vec();
        let version = v2.windows(5).position(|w| w == [0xa0, 3, 2, 1, 2]).unwrap();
        v2[version + 4] = 1;
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            STANDARD.encode(v2)
        );
        for cert in [pem.as_bytes(), b"invalid certificate"] {
            let mut config = config();
            config.auth_info.client_certificate_data = Some(STANDARD.encode(cert));
            assert!(client_builder(config, true, false).is_err());
        }
    }

    #[tokio::test]
    async fn certificate_and_key_files_work_and_inline_data_takes_precedence() {
        let mut config = config();
        config.auth_info.client_certificate_data = None;
        config.auth_info.client_key_data = None;
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/");
        config.auth_info.client_certificate = Some(format!("{dir}client-v1.pem"));
        config.auth_info.client_key = Some(format!("{dir}client.key"));
        assert!(client_builder(config.clone(), true, false).is_ok());
        assert!(client_builder(config.clone(), false, false).is_err());
        config.auth_info.client_certificate = Some("/missing/certificate".into());
        config.auth_info.client_key = Some("/missing/key".into());
        config.auth_info.client_certificate_data = Some(STANDARD.encode(CLIENT));
        config.auth_info.client_key_data = Some(STANDARD.encode(KEY).into());
        assert!(client_builder(config, true, false).is_ok());
    }

    // The test server trusts only the exact test certificates. It checks the
    // handshake signature with their public keys, including the v1 key.
    #[derive(Debug)]
    struct PinnedClient;

    impl ClientCertVerifier for PinnedClient {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(
            &self,
            cert: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: UnixTime,
        ) -> Result<ClientCertVerified, rustls::Error> {
            if [CLIENT, CLIENT_V3, CLIENT_P521, PROXY]
                .iter()
                .any(|pem| CertificateDer::from_pem_slice(pem).unwrap() == *cert)
            {
                Ok(ClientCertVerified::assertion())
            } else {
                Err(rustls::Error::General(
                    "unexpected test client certificate".into(),
                ))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            signature: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            self.verify_tls13_signature(message, cert, signature)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            signature: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            let cert = parse_certificate(cert).unwrap();
            rustls::crypto::verify_tls13_signature_with_raw_key(
                message,
                &SubjectPublicKeyInfoDer::from(cert.public_key().raw),
                signature,
                &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP521_SHA512,
            ]
        }
    }

    async fn server(
        pem: &[u8],
        version: &'static rustls::SupportedProtocolVersion,
    ) -> (Uri, JoinHandle<Result<String>>) {
        let tls = ServerConfig::builder_with_protocol_versions(&[version])
            .with_client_cert_verifier(Arc::new(PinnedClient))
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(pem).unwrap()],
                PrivateKeyDer::from_pem_slice(SERVER_KEY).unwrap(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/prefix", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(tls))
                .accept(stream)
                .await?;
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await?);
                ensure!(request.len() < 16384, "test request is too large");
            }
            let body = r#"{"major":"1","minor":"35","gitVersion":"v1.35.0","gitCommit":"test","gitTreeState":"clean","buildDate":"2026-01-01T00:00:00Z","goVersion":"go1.25","compiler":"gc","platform":"linux/amd64"}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
            stream.shutdown().await?;
            Ok(String::from_utf8(request)?)
        });
        (url, task)
    }

    #[tokio::test]
    async fn empty_inline_token_preserves_certificate_auth() {
        for client_pem in [CLIENT_V3, CLIENT] {
            for disabled in [false, true] {
                for token in [None, Some(""), Some("test-token")] {
                    let (url, task) = server(SERVER, &rustls::version::TLS13).await;
                    let mut config = config();
                    config.cluster_url = url;
                    config.auth_info.client_certificate_data = Some(STANDARD.encode(client_pem));
                    config.auth_info.token = token.map(Into::into);
                    let client =
                        crate::k8s::build_client(config, client_pem == CLIENT, disabled).unwrap();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        client.apiserver_version(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    let request = task.await.unwrap().unwrap();
                    let authorization = request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim())
                    });
                    assert_eq!(
                        authorization,
                        token
                            .filter(|token| !token.is_empty())
                            .map(|_| "Bearer test-token"),
                        "token: {token:?}, legacy certificate: {}, resumption disabled: {disabled}",
                        client_pem == CLIENT,
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn disabling_resumption_keeps_client_auth_on_new_connections() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for (server_pem, ca, client_pem) in [
                (SERVER, CA, CLIENT_V3),
                (SERVER, CA, CLIENT),
                (PROXY, PROXY, CLIENT_V3),
            ] {
                for disabled in [false, true] {
                    let tls = ServerConfig::builder_with_protocol_versions(&[version])
                        .with_client_cert_verifier(Arc::new(PinnedClient))
                        .with_single_cert(
                            vec![CertificateDer::from_pem_slice(server_pem).unwrap()],
                            PrivateKeyDer::from_pem_slice(SERVER_KEY).unwrap(),
                        )
                        .unwrap();
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut config = config();
                    config.cluster_url = format!("https://{}", listener.local_addr().unwrap())
                        .parse()
                        .unwrap();
                    config.root_cert =
                        Some(vec![CertificateDer::from_pem_slice(ca).unwrap().to_vec()]);
                    config.auth_info.client_certificate_data = Some(STANDARD.encode(client_pem));
                    let task = tokio::spawn(async move {
                        let mut resumed = Vec::new();
                        for _ in 0..3 {
                            let (socket, _) = listener.accept().await.unwrap();
                            let mut stream = acceptor.accept(socket).await.unwrap();
                            let reused = stream.get_ref().1.handshake_kind()
                                == Some(rustls::HandshakeKind::Resumed);
                            resumed.push(reused);
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") {
                                request.push(stream.read_u8().await.unwrap());
                                assert!(request.len() < 16384);
                            }
                            // Model an endpoint that loses the client identity on resumption.
                            let (status, body) = if reused {
                                (
                                    "401 Unauthorized",
                                    r#"{"kind":"Status","status":"Failure","reason":"Unauthorized","code":401}"#,
                                )
                            } else {
                                ("200 OK", "{}")
                            };
                            stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                            stream.shutdown().await.unwrap();
                        }
                        resumed
                    });
                    let client =
                        crate::k8s::build_client(config, client_pem == CLIENT, disabled).unwrap();
                    for attempt in 0..3 {
                        let request = Request::get("/version").body(Vec::new()).unwrap();
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            client.request::<serde_json::Value>(request),
                        )
                        .await
                        .unwrap();
                        if disabled || attempt == 0 {
                            assert!(result.is_ok(), "{result:?}");
                        } else {
                            assert!(
                                matches!(result, Err(kube::Error::Api(status)) if status.code == 401)
                            );
                        }
                    }
                    assert_eq!(task.await.unwrap(), vec![false, !disabled, !disabled]);
                }
            }
        }
    }

    #[tokio::test]
    async fn v1_and_v3_authenticate_with_tls12_and_tls13() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for pem in [CLIENT, CLIENT_V3] {
                let (url, task) = server(SERVER, version).await;
                let mut config = config();
                config.cluster_url = url;
                config.auth_info.client_certificate_data = Some(STANDARD.encode(pem));
                config.auth_info.impersonate = Some("test-user".into());
                let client = crate::k8s::build_client(config, true, false).unwrap();
                assert_eq!(client.default_namespace(), "test-ns");
                let info = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.apiserver_version(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(info.git_version, "v1.35.0");
                let request = task.await.unwrap().unwrap();
                assert!(request.starts_with("GET /prefix/version "), "{request}");
                assert!(request.contains("impersonate-user: test-user"), "{request}");
            }
        }
    }

    #[tokio::test]
    async fn p521_authenticates_with_tls12_and_tls13_without_tls_flags() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let (url, task) = server(SERVER, version).await;
            let mut config = config();
            config.cluster_url = url;
            config.auth_info.client_certificate_data = Some(STANDARD.encode(CLIENT_P521));
            config.auth_info.client_key_data = Some(STANDARD.encode(CLIENT_P521_KEY).into());
            let client = crate::k8s::build_client(config, false, false).unwrap();
            let info = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.apiserver_version(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(info.git_version, "v1.35.0");
            let request = task.await.unwrap().unwrap();
            assert!(request.starts_with("GET /prefix/version "), "{request}");
        }
    }

    #[tokio::test]
    async fn configured_ca_connects_with_v1_v3_and_ca_client_certificates() {
        for protocol in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for (pem, key) in [(CLIENT, KEY), (CLIENT_V3, KEY), (PROXY, SERVER_KEY)] {
                let (url, task) = server(PROXY, protocol).await;
                let mut config = config();
                config.cluster_url = url;
                config.root_cert = Some(vec![
                    CertificateDer::from_pem_slice(PROXY).unwrap().to_vec(),
                ]);
                config.auth_info.client_certificate_data = Some(STANDARD.encode(pem));
                config.auth_info.client_key_data = Some(STANDARD.encode(key).into());
                let client = crate::k8s::build_client(config, pem == CLIENT, false).unwrap();
                let info = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.apiserver_version(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(info.git_version, "v1.35.0");
                task.await.unwrap().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn configured_ca_loads_from_kubeconfig_file_and_data() {
        for inline in [false, true] {
            let (url, task) = server(PROXY, &rustls::version::TLS13).await;
            let mut cluster =
                serde_json::json!({"server": url.to_string(), "tls-server-name": "localhost"});
            if inline {
                cluster["certificate-authority-data"] = STANDARD.encode(PROXY).into();
            } else {
                cluster["certificate-authority"] = concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/tls/proxy-ca.pem"
                )
                .into();
            }
            let document = serde_json::json!({
                "apiVersion": "v1", "kind": "Config", "current-context": "proxy",
                "clusters": [{"name": "proxy", "cluster": cluster}],
                "contexts": [{"name": "proxy", "context": {"cluster": "proxy", "user": "proxy"}}],
                "users": [{"name": "proxy", "user": {
                    "client-certificate-data": STANDARD.encode(CLIENT_V3),
                    "client-key-data": STANDARD.encode(KEY)
                }}]
            });
            let kubeconfig = kube::config::Kubeconfig::from_yaml(&document.to_string()).unwrap();
            let config = Config::from_custom_kubeconfig(kubeconfig, &Default::default())
                .await
                .unwrap();
            let client = crate::k8s::build_client(config, false, false).unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.apiserver_version(),
            )
            .await
            .unwrap()
            .unwrap();
            task.await.unwrap().unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_credentials_keep_standard_resolution_and_expiration() {
        for disabled in [false, true] {
            let (url, task) = server(SERVER, &rustls::version::TLS13).await;
            let directory = std::env::temp_dir().join(format!(
                "sofka-exec-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&directory).unwrap();
            let counter = directory.join("count");
            let credential = |expiration| {
                serde_json::json!({
                    "apiVersion": "client.authentication.k8s.io/v1", "kind": "ExecCredential",
                    "status": {
                        "expirationTimestamp": expiration,
                        "clientCertificateData": std::str::from_utf8(CLIENT_V3).unwrap(),
                        "clientKeyData": std::str::from_utf8(KEY).unwrap()
                    }
                })
                .to_string()
            };
            let mut config = without_identity(&config());
            config.cluster_url = url;
            config
                .root_cert
                .as_mut()
                .unwrap()
                .push(CertificateDer::from_pem_slice(PROXY).unwrap().to_vec());
            config.auth_info.exec = Some(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "client.authentication.k8s.io/v1", "command": "sh",
                "args": ["-c", r#"
set -eu
count=0
if test -f "$1"; then read -r count < "$1"; fi
count=$((count + 1))
printf '%s\n' "$count" > "$1"
case "$count" in
    1|2) printf '%s' "$SOFKA_TEST_EXEC_FIRST" ;;
    3|4|5) printf '%s' "$SOFKA_TEST_EXEC_LAST" ;;
    *) exit 1 ;;
esac
"#, "sofka-exec-test", counter.to_str().unwrap()],
                "interactiveMode": "Never",
                "env": [
                    {"name": "SOFKA_TEST_EXEC_FIRST", "value": credential("2099-01-01T00:00:00Z")},
                    {"name": "SOFKA_TEST_EXEC_LAST", "value": credential("2098-01-01T00:00:00Z")}
                ]
            }))
            .unwrap(),
        );
            let result = crate::k8s::build_client(config, false, disabled);
            let count = std::fs::read_to_string(&counter).unwrap();
            std::fs::remove_dir_all(&directory).unwrap();
            let client = result.unwrap();
            // The default path resolves auth, TLS identity, and expiry once.
            // The opt-in path also needs the standard builder for expiry.
            assert_eq!(count.trim(), if disabled { "5" } else { "3" });
            assert_eq!(
                client.valid_until().unwrap().to_string(),
                "2098-01-01T00:00:00Z"
            );
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.apiserver_version(),
            )
            .await
            .unwrap()
            .unwrap();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn configured_ca_requires_the_server_private_key_in_tls12_and_tls13() {
        for protocol in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            let wrong_key = provider
                .key_provider
                .load_private_key(PrivateKeyDer::from_pem_slice(KEY).unwrap())
                .unwrap();
            let cert = CertifiedKey::new(
                vec![CertificateDer::from_pem_slice(PROXY).unwrap()],
                wrong_key,
            );
            let tls = ServerConfig::builder_with_protocol_versions(&[protocol])
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(SingleCertAndKey::from(cert)));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut config = without_identity(&config());
            config.cluster_url = format!("https://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap();
            config.root_cert = Some(vec![
                CertificateDer::from_pem_slice(PROXY).unwrap().to_vec(),
            ]);
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                tokio_rustls::TlsAcceptor::from(Arc::new(tls))
                    .accept(stream)
                    .await
            });
            let client = crate::k8s::build_client(config, false, false).unwrap();
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.apiserver_version(),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(format!("{error:?}").contains("BadSignature"), "{error:?}");
            assert!(task.await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn consent_preserves_server_trust_hostname_and_expiry_checks() {
        for disabled in [false, true] {
            for failure in ["trust", "hostname", "expiry"] {
                let pem = if failure == "expiry" { EXPIRED } else { SERVER };
                let (url, task) = server(pem, &rustls::version::TLS13).await;
                let mut config = config();
                config.cluster_url = url;
                match failure {
                    "trust" => config.root_cert = Some(vec![]),
                    "hostname" => config.tls_server_name = Some("wrong.example".into()),
                    _ => {}
                }
                let client = crate::k8s::build_client(config, true, disabled).unwrap();
                let error = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.apiserver_version(),
                )
                .await
                .unwrap()
                .unwrap_err();
                let error = format!("{:#}", anyhow::Error::new(error));
                let expected = match failure {
                    "trust" => "UnknownIssuer",
                    "hostname" => "not valid for name",
                    _ => "expired",
                };
                assert!(error.contains(expected), "{failure}: {error}");
                assert!(task.await.unwrap().is_err());
            }
        }
    }
    #[tokio::test]
    async fn client_certificates_work_through_http_and_socks5_proxies() {
        for (server_pem, client_pem, client_key, ca) in
            [(SERVER, CLIENT, KEY, CA), (PROXY, PROXY, SERVER_KEY, PROXY)]
        {
            for scheme in ["http", "socks5"] {
                let (url, server_task) = server(server_pem, &rustls::version::TLS13).await;
                let destination = url.authority().unwrap().to_string();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let proxy_url = format!("{scheme}://{}", listener.local_addr().unwrap())
                    .parse()
                    .unwrap();
                let proxy_task = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    if scheme == "http" {
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            request.push(stream.read_u8().await.unwrap());
                        }
                        let request = String::from_utf8(request).unwrap();
                        assert!(
                            request.starts_with(&format!("CONNECT {destination} ")),
                            "{request}"
                        );
                        stream
                            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                            .await
                            .unwrap();
                    } else {
                        assert_eq!(stream.read_u8().await.unwrap(), 5);
                        let count = stream.read_u8().await.unwrap();
                        let mut methods = vec![0; count as usize];
                        stream.read_exact(&mut methods).await.unwrap();
                        assert!(methods.contains(&0));
                        stream.write_all(&[5, 0]).await.unwrap();
                        let mut header = [0; 4];
                        stream.read_exact(&mut header).await.unwrap();
                        assert_eq!(&header[..3], &[5, 1, 0]);
                        let length = match header[3] {
                            1 => 4,
                            3 => stream.read_u8().await.unwrap() as usize,
                            4 => 16,
                            other => panic!("unexpected SOCKS address type: {other}"),
                        };
                        let mut address = vec![0; length + 2];
                        stream.read_exact(&mut address).await.unwrap();
                        stream
                            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                            .await
                            .unwrap();
                    }
                    let mut upstream = tokio::net::TcpStream::connect(destination).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                });
                let mut config = config();
                config.cluster_url = url;
                config.proxy_url = Some(proxy_url);
                config.root_cert = Some(vec![CertificateDer::from_pem_slice(ca).unwrap().to_vec()]);
                config.auth_info.client_certificate_data = Some(STANDARD.encode(client_pem));
                config.auth_info.client_key_data = Some(STANDARD.encode(client_key).into());
                let client = crate::k8s::build_client(config, true, false).unwrap();
                let version = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.apiserver_version(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(version.git_version, "v1.35.0");
                server_task.await.unwrap().unwrap();
                proxy_task.await.unwrap();
            }
        }
    }
}
