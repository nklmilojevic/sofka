//! Redacted incident-bundle assembly for `:bundle`.
//!
//! A diagnostic bundle gathers everything sofka knows about one object — its
//! (redacted) YAML, its owner, conditions, recent events, the incident
//! explanation, the session timeline, bounded logs, and a metrics snapshot —
//! into one Markdown document for handoff between application and platform
//! teams. The redaction here is the safety-critical part: Secret data and
//! credential-looking annotations never reach the file, and the manifest spells
//! out exactly what was included and what was withheld.

use kube::core::DynamicObject;
use serde_json::Value;

/// Placeholder substituted for any redacted value.
pub const REDACTED: &str = "«redacted»";

/// Annotation-key substrings that mark a value as credential-like. Matched
/// case-insensitively against the whole key.
const CREDENTIAL_HINTS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "apikey",
    "api-key",
    "credential",
    "private-key",
    "privatekey",
];

/// Redact one object's JSON in place, returning human-readable notes of what
/// was removed (for the bundle manifest). Deterministic and side-effect free.
///
/// Redacts: `managedFields` (noise), the `last-applied-configuration`
/// annotation (can embed a whole Secret), any annotation whose key looks like a
/// credential, and — for Secrets — every `data`/`stringData` value. Env vars
/// sourced from Secrets are counted and flagged (their values are references,
/// not literals, so nothing to strip).
pub fn redact_object(kind: &str, v: &mut Value) -> Vec<String> {
    let mut notes = Vec::new();

    if let Some(md) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        md.remove("managedFields");
        if let Some(ann) = md.get_mut("annotations").and_then(Value::as_object_mut) {
            for (k, val) in ann.iter_mut() {
                let kl = k.to_lowercase();
                if k == "kubectl.kubernetes.io/last-applied-configuration" {
                    *val = Value::String(REDACTED.into());
                    notes.push("annotation last-applied-configuration".into());
                } else if CREDENTIAL_HINTS.iter().any(|h| kl.contains(h)) {
                    *val = Value::String(REDACTED.into());
                    notes.push(format!("annotation {k}"));
                }
            }
        }
    }

    if kind == "Secret" {
        for field in ["data", "stringData"] {
            if let Some(obj) = v.get_mut(field).and_then(Value::as_object_mut) {
                let n = obj.len();
                for val in obj.values_mut() {
                    *val = Value::String(REDACTED.into());
                }
                if n > 0 {
                    notes.push(format!("Secret {field}: {n} value(s)"));
                }
            }
        }
    }

    let secret_env = count_secret_env(v);
    if secret_env > 0 {
        notes.push(format!(
            "{secret_env} env var(s) sourced from Secrets (values not included)"
        ));
    }

    notes
}

/// Count `env[*].valueFrom.secretKeyRef` across every container list in a pod
/// (or pod-template) spec, at any nesting depth.
fn count_secret_env(v: &Value) -> usize {
    let mut n = 0;
    walk(v, &mut n);
    return n;

    fn walk(v: &Value, n: &mut usize) {
        match v {
            Value::Object(map) => {
                if map.get("env").is_some_and(Value::is_array) {
                    for e in map["env"].as_array().unwrap() {
                        if e.pointer("/valueFrom/secretKeyRef").is_some() {
                            *n += 1;
                        }
                    }
                }
                for val in map.values() {
                    walk(val, n);
                }
            }
            Value::Array(arr) => {
                for val in arr {
                    walk(val, n);
                }
            }
            _ => {}
        }
    }
}

/// Redact an object and render it as YAML lines, plus the redaction notes.
/// `kind` seeds Secret detection when the object carries no `kind` of its own.
pub fn redact_to_yaml(obj: &DynamicObject, kind: &str) -> (Vec<String>, Vec<String>) {
    let mut v = serde_json::to_value(obj).unwrap_or(Value::Null);
    let effective_kind = if kind.is_empty() {
        v.get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        kind.to_string()
    };
    let notes = redact_object(&effective_kind, &mut v);
    let yaml = serde_yaml::to_string(&v).unwrap_or_else(|e| format!("# error: {e}"));
    (yaml.lines().map(String::from).collect(), notes)
}

/// A Markdown document builder for the bundle. Sections are appended in order;
/// [`Self::finish`] returns the full text.
#[derive(Default)]
pub struct Doc {
    out: String,
}

impl Doc {
    pub fn new(title: &str) -> Self {
        let mut d = Doc::default();
        d.out.push_str(&format!("# {title}\n"));
        d
    }

    /// A `key: value` metadata line (used for the header block).
    pub fn field(&mut self, key: &str, value: &str) {
        self.out.push_str(&format!("- **{key}:** {value}\n"));
    }

    /// A `## heading`.
    pub fn heading(&mut self, h: &str) {
        self.out.push_str(&format!("\n## {h}\n\n"));
    }

    /// A bullet list; renders a `_none_` placeholder when empty.
    pub fn bullets(&mut self, items: &[String]) {
        if items.is_empty() {
            self.out.push_str("_none_\n");
            return;
        }
        for i in items {
            self.out.push_str(&format!("- {i}\n"));
        }
    }

    /// A fenced code block; renders a `_none_` placeholder when empty.
    pub fn code(&mut self, lang: &str, lines: &[String]) {
        if lines.is_empty() {
            self.out.push_str("_none_\n");
            return;
        }
        self.out.push_str(&format!("```{lang}\n"));
        for l in lines {
            self.out.push_str(l);
            self.out.push('\n');
        }
        self.out.push_str("```\n");
    }

    pub fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_secret_data_and_stringdata() {
        let mut v = json!({
            "kind": "Secret",
            "metadata": {"name": "creds"},
            "data": {"password": "aHVudGVyMg==", "user": "YWRtaW4="},
            "stringData": {"token": "plain-text-token"}
        });
        let notes = redact_object("Secret", &mut v);
        assert_eq!(v["data"]["password"], REDACTED);
        assert_eq!(v["data"]["user"], REDACTED);
        assert_eq!(v["stringData"]["token"], REDACTED);
        // The literal secret values must be gone from the serialized form.
        let dumped = v.to_string();
        assert!(!dumped.contains("aHVudGVyMg=="), "{dumped}");
        assert!(!dumped.contains("plain-text-token"), "{dumped}");
        assert!(notes.iter().any(|n| n.contains("data: 2")));
        assert!(notes.iter().any(|n| n.contains("stringData: 1")));
    }

    #[test]
    fn redacts_credential_annotations_and_last_applied() {
        let mut v = json!({
            "kind": "ConfigMap",
            "metadata": {"name": "cm", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"x\":\"y\"}}",
                "example.com/api-token": "abc123",
                "harmless": "keep-me"
            }}
        });
        let notes = redact_object("ConfigMap", &mut v);
        let ann = &v["metadata"]["annotations"];
        assert_eq!(
            ann["kubectl.kubernetes.io/last-applied-configuration"],
            REDACTED
        );
        assert_eq!(ann["example.com/api-token"], REDACTED);
        assert_eq!(ann["harmless"], "keep-me", "non-credential kept");
        assert!(notes.iter().any(|n| n.contains("last-applied")));
    }

    #[test]
    fn drops_managed_fields_and_counts_secret_env() {
        let mut v = json!({
            "kind": "Pod",
            "metadata": {"name": "p", "managedFields": [{"manager": "kubelet"}]},
            "spec": {"containers": [{"name": "c", "env": [
                {"name": "PW", "valueFrom": {"secretKeyRef": {"name": "s", "key": "pw"}}},
                {"name": "PLAIN", "value": "hi"}
            ]}]}
        });
        let notes = redact_object("Pod", &mut v);
        assert!(v["metadata"].get("managedFields").is_none());
        assert!(notes.iter().any(|n| n.contains("1 env var")), "{notes:?}");
    }

    #[test]
    fn non_secret_data_is_not_touched() {
        let mut v = json!({
            "kind": "ConfigMap",
            "metadata": {"name": "cm"},
            "data": {"key": "value"}
        });
        redact_object("ConfigMap", &mut v);
        assert_eq!(v["data"]["key"], "value", "ConfigMap data is plain config");
    }

    #[test]
    fn doc_renders_sections_and_placeholders() {
        let mut d = Doc::new("incident");
        d.field("resource", "pods/api");
        d.heading("Events");
        d.bullets(&[]);
        d.heading("YAML");
        d.code("yaml", &["kind: Pod".into()]);
        let out = d.finish();
        assert!(out.starts_with("# incident\n"));
        assert!(out.contains("**resource:** pods/api"));
        assert!(out.contains("## Events\n\n_none_"));
        assert!(out.contains("```yaml\nkind: Pod\n```"));
    }
}
