use kube::{
    Config,
    config::{KubeConfigOptions, Kubeconfig, KubeconfigError},
};
use secrecy::ExposeSecret;

pub(crate) async fn from_custom(
    mut kubeconfig: Kubeconfig,
    options: &KubeConfigOptions,
) -> Result<Config, KubeconfigError> {
    // Go's base64 decoder ignores CR and LF, including after YAML !!binary decoding.
    for entry in &mut kubeconfig.clusters {
        if let Some(cluster) = &mut entry.cluster
            && let Some(data) = &mut cluster.certificate_authority_data
        {
            strip_line_breaks(data);
        }
    }
    for entry in &mut kubeconfig.auth_infos {
        if let Some(auth) = &mut entry.auth_info {
            if let Some(data) = &mut auth.client_certificate_data {
                strip_line_breaks(data);
            }
            if let Some(data) = &auth.client_key_data
                && data.expose_secret().contains(['\r', '\n'])
            {
                let mut normalized = data.expose_secret().to_owned();
                strip_line_breaks(&mut normalized);
                auth.client_key_data = Some(normalized.into());
            }
        }
    }
    Config::from_custom_kubeconfig(kubeconfig, options).await
}

pub(crate) async fn infer() -> Result<Config, kube::config::InferConfigError> {
    if let Ok(kubeconfig) = Kubeconfig::read()
        && let Ok(mut config) = from_custom(kubeconfig, &KubeConfigOptions::default()).await
    {
        config.apply_debug_overrides();
        return Ok(config);
    }
    // Keep the upstream in-cluster fallback and combined error for invalid configs.
    Config::infer().await
}

fn strip_line_breaks(data: &mut String) {
    data.retain(|c| c != '\r' && c != '\n');
}

#[cfg(test)]
pub(crate) fn fixture(binary: bool, wrapped: bool) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    fn wrap(value: &str) -> String {
        value
            .as_bytes()
            .chunks(76)
            .map(|line| format!("{}\r\n", std::str::from_utf8(line).unwrap()))
            .collect()
    }
    let mut yaml = String::from(
        "apiVersion: v1\nkind: Config\ncurrent-context: target\nclusters:\n- name: c\n  cluster:\n    server: https://127.0.0.1:1\n",
    );
    for (field, pem) in [
        (
            "certificate-authority-data",
            include_bytes!("../../tests/fixtures/tls/ca.pem").as_slice(),
        ),
        (
            "client-certificate-data",
            include_bytes!("../../tests/fixtures/tls/client.pem").as_slice(),
        ),
        (
            "client-key-data",
            include_bytes!("../../tests/fixtures/tls/client.key").as_slice(),
        ),
    ] {
        if field == "client-certificate-data" {
            yaml.push_str("users:\n- name: u\n  user:\n");
        }
        let mut data = STANDARD.encode(pem);
        if wrapped {
            data = wrap(&data);
        }
        if binary {
            data = wrap(&STANDARD.encode(data));
        }
        yaml.push_str(&format!(
            "    {field}: {}|-\n",
            if binary { "!!binary " } else { "" }
        ));
        for line in data.lines() {
            yaml.push_str(&format!("      {line}\n"));
        }
    }
    yaml.push_str("contexts:\n- name: target\n  context:\n    cluster: c\n    user: u\n");
    yaml
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrapped_certificate_data_builds_a_client() {
        for binary in [false, true] {
            for wrapped in [false, true] {
                let config = Kubeconfig::from_yaml(&fixture(binary, wrapped)).unwrap();
                let config = from_custom(config, &Default::default()).await.unwrap();
                assert!(!config.root_cert.as_ref().unwrap().is_empty());
                super::super::build_client(config, false, false).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn invalid_base64_is_still_rejected() {
        for invalid in [' ', '\t', '!'] {
            for field in ["ca", "certificate", "key"] {
                let mut config = Kubeconfig::from_yaml(&fixture(false, false)).unwrap();
                let auth = config.auth_infos[0].auth_info.as_mut().unwrap();
                match field {
                    "ca" => config.clusters[0]
                        .cluster
                        .as_mut()
                        .unwrap()
                        .certificate_authority_data
                        .as_mut()
                        .unwrap()
                        .insert(4, invalid),
                    "certificate" => auth
                        .client_certificate_data
                        .as_mut()
                        .unwrap()
                        .insert(4, invalid),
                    _ => {
                        let mut data = auth
                            .client_key_data
                            .as_ref()
                            .unwrap()
                            .expose_secret()
                            .to_owned();
                        data.insert(4, invalid);
                        auth.client_key_data = Some(data.into());
                    }
                }
                let result = from_custom(config, &Default::default()).await;
                if field == "ca" {
                    assert!(result.is_err());
                } else {
                    assert!(super::super::build_client(result.unwrap(), false, false).is_err());
                }
            }
        }
    }
}
