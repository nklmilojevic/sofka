use super::*;

/// Read completion values without starting watches or loading TUI state.
pub async fn values(
    kubeconfig: Kubeconfig,
    context: Option<&str>,
    namespaces: bool,
    allow_v1_client_cert: bool,
    no_tls_resumption: bool,
) -> Result<Vec<String>> {
    let options = KubeConfigOptions {
        context: context.map(str::to_owned),
        ..Default::default()
    };
    let mut config = kubeconfig::from_custom(kubeconfig.clone(), &options).await?;
    proxy::configure(&mut config, &kubeconfig, context);
    let client = build_client(config, allow_v1_client_cert, no_tls_resumption)?;
    if namespaces {
        let api = Api::<k8s_openapi::api::core::v1::Namespace>::all(client);
        let list = api.list(&ListParams::default()).await?;
        return Ok(list
            .items
            .into_iter()
            .filter_map(|n| n.metadata.name)
            .collect());
    }
    let discovered = discovery::discover(&client).await?;
    let mut values = Vec::new();
    for resource in discovered.resources {
        let ar = resource.kind.ar;
        values.push(ar.plural.clone());
        values.push(ar.kind.to_lowercase());
        if !ar.group.is_empty() {
            values.push(format!("{}.{}", ar.plural, ar.group));
        }
        values.extend(resource.short_names);
    }
    Ok(values)
}
