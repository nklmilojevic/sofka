//! Decoding of Helm release storage `Secret`s.
//!
//! Helm (the package manager, not Flux's `HelmRelease` CRD) stores each
//! release revision as a `Secret` named `sh.helm.release.v1.<release>.v<rev>`
//! with `type: helm.sh/release.v1` and labels `owner=helm`, `name=<release>`,
//! `version=<revision>`, `status=<status>`. `data.release` is the release
//! JSON, base64-encoded and gzip-compressed by Helm itself, then base64
//! re-encoded by Kubernetes' own wire JSON for `Secret.data` bytes — so
//! decoding needs base64 twice, then gunzip, then JSON
//! (see helm/helm `pkg/storage/driver/util.go`).
//!
//! Only the Secret storage driver is supported (Helm's default; the
//! ConfigMap driver is out of scope).

use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde::Deserialize;
use serde_json::Value;

/// A decoded Helm release revision. The k8s namespace is deliberately not
/// carried here — callers already have the storage Secret's own namespace,
/// which is the trustworthy source (not whatever the embedded JSON claims).
pub struct Release {
    pub name: String,
    pub revision: i64,
    pub status: String,
    pub chart_name: String,
    pub chart_version: String,
    pub app_version: String,
    pub description: String,
    /// `info.last_deployed` as a unix timestamp, when parseable.
    pub last_deployed_secs: Option<i64>,
    pub notes: String,
    /// User-supplied value overrides (`helm install/upgrade -f`/`--set`) —
    /// the "values" k9s' History-view Enter shows. The chart's own default
    /// `values.yaml` ("all values") isn't surfaced; see the plan's explicit
    /// scope note.
    pub config: Value,
    pub manifest: String,
}

#[derive(Deserialize, Default)]
struct RawRelease {
    #[serde(default)]
    name: String,
    #[serde(default)]
    info: RawInfo,
    #[serde(default)]
    chart: RawChart,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    manifest: String,
    #[serde(default)]
    version: i64,
}

#[derive(Deserialize, Default)]
struct RawInfo {
    #[serde(default)]
    last_deployed: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    notes: String,
}

#[derive(Deserialize, Default)]
struct RawChart {
    #[serde(default)]
    metadata: RawMetadata,
}

#[derive(Deserialize, Default)]
struct RawMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "appVersion")]
    app_version: String,
}

/// Decode a release Secret into its `Release`. `None` if the secret doesn't
/// carry a `data.release` payload or it can't be decoded (corrupt, unknown
/// format, or not a Helm release secret at all) — callers treat that as
/// "unrenderable", not a crash.
pub fn decode(secret: &DynamicObject) -> Option<Release> {
    let wire = secret.data.pointer("/data/release")?.as_str()?;
    let helm_encoded = BASE64.decode(wire).ok()?;
    let gzipped = BASE64.decode(helm_encoded).ok()?;
    let mut gz = flate2::read::GzDecoder::new(&gzipped[..]);
    let mut json = Vec::new();
    gz.read_to_end(&mut json).ok()?;
    let raw: RawRelease = serde_json::from_slice(&json).ok()?;

    let last_deployed_secs = raw
        .info
        .last_deployed
        .as_deref()
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map(|ts| ts.as_second());

    Some(Release {
        name: raw.name,
        revision: raw.version,
        status: raw.info.status,
        chart_name: raw.chart.metadata.name,
        chart_version: raw.chart.metadata.version,
        app_version: raw.chart.metadata.app_version,
        description: raw.info.description,
        last_deployed_secs,
        notes: raw.info.notes,
        config: raw.config,
        manifest: raw.manifest,
    })
}

/// The release name from the secret's `name` label — cheap, no decode needed.
pub fn release_name(secret: &DynamicObject) -> Option<&str> {
    secret
        .metadata
        .labels
        .as_ref()?
        .get("name")
        .map(String::as_str)
}

/// The revision number from the secret's `version` label — cheap, no decode
/// needed. Used to pick the latest revision per release without decoding
/// every revision's payload.
pub fn revision(secret: &DynamicObject) -> Option<i64> {
    secret
        .metadata
        .labels
        .as_ref()?
        .get("version")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn fixture_secret(release_json: &str) -> DynamicObject {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(release_json.as_bytes()).unwrap();
        let gzipped = gz.finish().unwrap();
        let helm_b64 = BASE64.encode(gzipped);
        let wire_b64 = BASE64.encode(helm_b64);

        let data: Value = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": "sh.helm.release.v1.myrelease.v2",
                "namespace": "default",
                "labels": {
                    "owner": "helm",
                    "name": "myrelease",
                    "version": "2",
                    "status": "deployed",
                },
            },
            "type": "helm.sh/release.v1",
            "data": { "release": wire_b64 },
        }))
        .unwrap();

        serde_json::from_value(data).unwrap()
    }

    #[test]
    fn decodes_a_release_secret() {
        let secret = fixture_secret(
            r#"{
                "name": "myrelease",
                "namespace": "default",
                "version": 2,
                "info": {
                    "status": "deployed",
                    "description": "Upgrade complete",
                    "last_deployed": "2024-01-15T10:30:00Z",
                    "notes": "Thanks for installing!"
                },
                "chart": {
                    "metadata": { "name": "mychart", "version": "1.2.3", "appVersion": "4.5.6" },
                    "values": { "replicaCount": 1 }
                },
                "config": { "replicaCount": 3 },
                "manifest": "apiVersion: v1\nkind: ConfigMap\n"
            }"#,
        );

        let rel = decode(&secret).expect("should decode");
        assert_eq!(rel.name, "myrelease");
        assert_eq!(rel.revision, 2);
        assert_eq!(rel.status, "deployed");
        assert_eq!(rel.chart_name, "mychart");
        assert_eq!(rel.chart_version, "1.2.3");
        assert_eq!(rel.app_version, "4.5.6");
        assert_eq!(rel.description, "Upgrade complete");
        assert_eq!(rel.notes, "Thanks for installing!");
        assert!(rel.manifest.contains("ConfigMap"));
        assert_eq!(
            rel.config.get("replicaCount").and_then(Value::as_i64),
            Some(3)
        );
        assert!(rel.last_deployed_secs.is_some());

        assert_eq!(release_name(&secret), Some("myrelease"));
        assert_eq!(revision(&secret), Some(2));
    }

    #[test]
    fn decode_returns_none_for_non_release_secret() {
        let data: Value = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "some-secret", "namespace": "default" },
            "data": { "password": "aHVudGVyMg==" },
        });
        let secret: DynamicObject = serde_json::from_value(data).unwrap();
        assert!(decode(&secret).is_none());
    }
}
