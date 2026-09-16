//! Accept a configured CA as the server certificate without disabling TLS checks.

use std::sync::Arc;

use kube::Config;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_name,
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use x509_parser::{
    extensions::{ExtendedKeyUsage, KeyUsage},
    oid_registry::{OID_X509_EXT_EXTENDED_KEY_USAGE, OID_X509_EXT_KEY_USAGE},
    prelude::*,
};

pub(crate) fn configured_roots(config: &Config) -> Option<&[Vec<u8>]> {
    // These paths must retain kube-rs credential resolution and CA reload behavior.
    if config.accept_invalid_certs
        || config.root_cert_file.is_some()
        || config.auth_info.exec.is_some()
        || config
            .auth_info
            .auth_provider
            .as_ref()
            .is_some_and(|provider| provider.name != "oidc")
    {
        return None;
    }
    config.root_cert.as_deref().filter(|roots| {
        // A CA without a server name cannot pass hostname verification as a leaf.
        // Keep ordinary kubeconfigs on the standard client construction path.
        roots.iter().any(|der| {
            X509Certificate::from_der(der).is_ok_and(|(_, cert)| {
                cert.is_ca() && cert.subject_alternative_name().ok().flatten().is_some()
            })
        })
    })
}

pub(crate) fn install(tls: &mut ClientConfig, roots: &[Vec<u8>]) -> anyhow::Result<()> {
    let verifier = ConfiguredCaVerifier::new(roots, tls.crypto_provider().clone())?;
    tls.dangerous().set_certificate_verifier(Arc::new(verifier));
    Ok(())
}

#[derive(Debug)]
struct ConfiguredCaVerifier {
    roots: Vec<CertificateDer<'static>>,
    standard: Arc<WebPkiServerVerifier>,
}

impl ConfiguredCaVerifier {
    fn new(
        roots: &[Vec<u8>],
        provider: Arc<rustls::crypto::CryptoProvider>,
    ) -> anyhow::Result<Self> {
        let roots: Vec<_> = roots.iter().cloned().map(CertificateDer::from).collect();
        let mut store = RootCertStore::empty();
        for root in &roots {
            store.add(root.clone())?;
        }
        let standard =
            WebPkiServerVerifier::builder_with_provider(Arc::new(store), provider).build()?;
        Ok(Self { roots, standard })
    }
}

impl ServerCertVerifier for ConfiguredCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let error = match self.standard.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(error) => error,
        };
        // WebPKI checks DER, critical extensions, and validity before this error.
        // No other verification failure qualifies for the exception.
        if !matches!(
            &error,
            Error::InvalidCertificate(CertificateError::Other(other))
                if other.0.downcast_ref::<webpki::Error>() == Some(&webpki::Error::CaUsedAsEndEntity)
        ) || !self.roots.iter().any(|root| root == end_entity)
        {
            return Err(error);
        }

        check_usage(end_entity)?;
        verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
        // The exact DER match supplies trust. The TLS signature checks below still
        // require the peer to prove that it has the corresponding private key.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.standard.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.standard.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.standard.supported_verify_schemes()
    }
}

fn check_usage(der: &[u8]) -> Result<(), Error> {
    let bad_encoding = || Error::InvalidCertificate(CertificateError::BadEncoding);
    let (rest, cert) = X509Certificate::from_der(der).map_err(|_| bad_encoding())?;
    if !rest.is_empty() {
        return Err(bad_encoding());
    }
    cert.extensions_map().map_err(|_| bad_encoding())?;
    // Name constraints require path validation. Do not bypass them through a pin.
    if cert
        .name_constraints()
        .map_err(|_| bad_encoding())?
        .is_some()
    {
        return Err(CertificateError::UnhandledCriticalExtension.into());
    }
    if let Some(extension) = cert
        .get_extension_unique(&OID_X509_EXT_KEY_USAGE)
        .map_err(|_| bad_encoding())?
    {
        let (rest, usage) = KeyUsage::from_der(extension.value).map_err(|_| bad_encoding())?;
        if !rest.is_empty() {
            return Err(bad_encoding());
        }
        if !usage.digital_signature() {
            return Err(CertificateError::InvalidPurpose.into());
        }
    }
    if let Some(extension) = cert
        .get_extension_unique(&OID_X509_EXT_EXTENDED_KEY_USAGE)
        .map_err(|_| bad_encoding())?
    {
        let (rest, usage) =
            ExtendedKeyUsage::from_der(extension.value).map_err(|_| bad_encoding())?;
        if !rest.is_empty() {
            return Err(bad_encoding());
        }
        if !usage.server_auth && !usage.any {
            return Err(CertificateError::InvalidPurpose.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;
    use std::time::Duration;

    const PROXY: &[u8] = include_bytes!("../tests/fixtures/tls/proxy-ca.pem");
    const SERVER_AUTH: &[u8] = include_bytes!("../tests/fixtures/tls/proxy-ca-server-auth.pem");
    const CA: &[u8] = include_bytes!("../tests/fixtures/tls/ca.pem");
    const SERVER: &[u8] = include_bytes!("../tests/fixtures/tls/server.pem");
    const NOW: u64 = 1_800_000_000;

    fn verifier(roots: &[&[u8]]) -> ConfiguredCaVerifier {
        let roots: Vec<_> = roots
            .iter()
            .map(|pem| CertificateDer::from_pem_slice(pem).unwrap().to_vec())
            .collect();
        ConfiguredCaVerifier::new(
            &roots,
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .unwrap()
    }

    fn verify(
        verifier: &ConfiguredCaVerifier,
        pem: &[u8],
        name: &str,
        time: u64,
    ) -> Result<ServerCertVerified, Error> {
        verifier.verify_server_cert(
            &CertificateDer::from_pem_slice(pem).unwrap(),
            &[],
            &ServerName::try_from(name).unwrap(),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(time)),
        )
    }

    #[test]
    fn configured_ca_accepts_dns_ip_and_wildcard_names() {
        for pem in [PROXY, SERVER_AUTH] {
            let verifier = verifier(&[CA, pem]);
            for name in ["localhost", "127.0.0.1", "cluster.proxy.example"] {
                verify(&verifier, pem, name, NOW).unwrap();
            }
            for name in ["wrong.example", "127.0.0.2", "a.b.proxy.example"] {
                assert!(verify(&verifier, pem, name, NOW).is_err(), "{name}");
            }
        }
    }

    #[test]
    fn configured_ca_still_checks_validity_dates() {
        let verifier = verifier(&[PROXY]);
        for time in [1_500_000_000, 5_000_000_000] {
            let error = verify(&verifier, PROXY, "localhost", time).unwrap_err();
            assert!(
                matches!(
                    error,
                    Error::InvalidCertificate(
                        CertificateError::ExpiredContext { .. }
                            | CertificateError::NotValidYetContext { .. }
                    )
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_shared_key_or_subject_does_not_replace_an_exact_certificate_match() {
        let verifier = verifier(&[PROXY]);
        let error = verify(&verifier, SERVER_AUTH, "localhost", NOW).unwrap_err();
        assert!(matches!(
            error,
            Error::InvalidCertificate(CertificateError::Other(_))
        ));
        assert!(verify(&verifier, SERVER, "localhost", NOW).is_err());
    }

    #[test]
    fn configured_ca_does_not_skip_usage_or_extension_checks() {
        for pem in [
            include_bytes!("../tests/fixtures/tls/proxy-ca-client-auth.pem").as_slice(),
            include_bytes!("../tests/fixtures/tls/proxy-ca-key-cert-sign.pem").as_slice(),
            include_bytes!("../tests/fixtures/tls/proxy-ca-critical.pem").as_slice(),
            include_bytes!("../tests/fixtures/tls/proxy-ca-malformed-ku.pem").as_slice(),
            include_bytes!("../tests/fixtures/tls/proxy-ca-constrained.pem").as_slice(),
        ] {
            assert!(verify(&verifier(&[pem]), pem, "localhost", NOW).is_err());
        }
    }

    #[test]
    fn ordinary_server_certificates_keep_standard_verification() {
        let verifier = verifier(&[CA, PROXY]);
        verify(&verifier, SERVER, "localhost", NOW).unwrap();
        assert!(verify(&verifier, SERVER, "wrong.example", NOW).is_err());
        assert!(verify(&verifier, PROXY, "localhost", NOW).is_ok());
        let untrusted = verifier.standard.verify_server_cert(
            &CertificateDer::from_pem_slice(PROXY).unwrap(),
            &[],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(NOW)),
        );
        assert!(matches!(
            untrusted,
            Err(Error::InvalidCertificate(CertificateError::Other(_)))
        ));
    }

    #[test]
    fn platform_trust_insecure_mode_and_ca_reload_keep_the_kube_verifier() {
        let mut config = Config::new("https://localhost".parse().unwrap());
        assert!(configured_roots(&config).is_none());
        config.root_cert = Some(vec![CertificateDer::from_pem_slice(CA).unwrap().to_vec()]);
        assert!(configured_roots(&config).is_none());
        config.root_cert = Some(vec![
            CertificateDer::from_pem_slice(PROXY).unwrap().to_vec(),
        ]);
        assert!(configured_roots(&config).is_some());
        config.accept_invalid_certs = true;
        assert!(configured_roots(&config).is_none());
        config.accept_invalid_certs = false;
        config.root_cert_file = Some("/test/ca.pem".into());
        assert!(configured_roots(&config).is_none());
    }

    #[test]
    fn only_oidc_providers_allow_the_configured_ca_exception() {
        let mut config = Config::new("https://localhost".parse().unwrap());
        config.root_cert = Some(vec![
            CertificateDer::from_pem_slice(PROXY).unwrap().to_vec(),
        ]);
        for name in ["oidc", "gcp", "azure", "unknown"] {
            config.auth_info.auth_provider = Some(kube::config::AuthProviderConfig {
                name: name.into(),
                ..Default::default()
            });
            assert_eq!(configured_roots(&config).is_some(), name == "oidc");
        }
        config.auth_info.auth_provider.as_mut().unwrap().name = "oidc".into();
        config.auth_info.exec = Some(kube::config::ExecConfig::default());
        assert!(configured_roots(&config).is_none());
        config.auth_info.exec = None;
        config.accept_invalid_certs = true;
        assert!(configured_roots(&config).is_none());
        config.accept_invalid_certs = false;
        config.root_cert_file = Some("/test/ca.pem".into());
        assert!(configured_roots(&config).is_none());
    }
}
