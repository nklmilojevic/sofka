use super::*;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    SelfSubjectRulesReview, SelfSubjectRulesReviewSpec,
};

impl App {
    /// `:can-i` — an overview of what the current identity may do in the active
    /// namespace, from a `SelfSubjectRulesReview`. Rendered as a scrollable
    /// document (arrives as [`Msg::Detail`]).
    pub(super) fn open_can_i(&mut self) {
        self.set_return_mode();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let ns = if self.namespace.is_empty() {
            "default".to_string()
        } else {
            self.namespace.clone()
        };
        let user = self.cluster.context.clone();
        self.flash = format!("can-i: reviewing rules in {ns}…");
        self.flash_err = false;
        tokio::spawn(async move {
            let review = SelfSubjectRulesReview {
                spec: SelfSubjectRulesReviewSpec {
                    namespace: Some(ns.clone()),
                },
                ..Default::default()
            };
            let api: Api<SelfSubjectRulesReview> = Api::all(client);
            let msg = match api.create(&kube::api::PostParams::default(), &review).await {
                Ok(resp) => Msg::Detail {
                    generation: genr,
                    title: format!("can-i · {user} · namespace {ns}"),
                    lines: format_rules(resp.status, &ns),
                    warn: None,
                },
                Err(e) => Msg::Detail {
                    generation: genr,
                    title: format!("can-i · namespace {ns}"),
                    lines: vec![format!("access review failed: {e}")],
                    warn: Some("could not review permissions".into()),
                },
            };
            let _ = tx.send(msg).await;
        });
    }

    /// `:can-i <verb> <resource> [namespace]` — a single access check via
    /// `SelfSubjectAccessReview`, answered in the status line.
    pub(super) fn check_can_i(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        let (Some(verb), Some(resource)) = (parts.next(), parts.next()) else {
            self.flash_warn("usage: :can-i <verb> <resource> [namespace]");
            return;
        };
        let verb = verb.to_string();
        // Resolve the resource so aliases (`deploy`) and group are correct;
        // fall back to the raw text when it isn't a known kind.
        let (group, res) = match self.cluster.resolve(resource) {
            Some(k) => (k.ar.group.clone(), k.ar.plural.to_lowercase()),
            None => (String::new(), resource.to_lowercase()),
        };
        let ns = parts
            .next()
            .map(normalize_ns)
            .unwrap_or_else(|| self.namespace.clone());
        let scope = if ns.is_empty() {
            "cluster-wide".to_string()
        } else {
            format!("in {ns}")
        };
        let subject = format!("{verb} {res} {scope}");

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = format!("can-i {subject}…");
        self.flash_err = false;
        tokio::spawn(async move {
            let review = SelfSubjectAccessReview {
                spec: SelfSubjectAccessReviewSpec {
                    resource_attributes: Some(ResourceAttributes {
                        verb: Some(verb),
                        resource: Some(res),
                        group: (!group.is_empty()).then_some(group),
                        namespace: (!ns.is_empty()).then_some(ns),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            let api: Api<SelfSubjectAccessReview> = Api::all(client);
            let msg = match api.create(&kube::api::PostParams::default(), &review).await {
                Ok(resp) => {
                    let status = resp.status.unwrap_or_default();
                    let reason = status.reason.filter(|r| !r.is_empty());
                    // `denied` overrides `allowed` (an explicit deny wins).
                    if status.denied.unwrap_or(false) {
                        deny_msg(genr, &subject, reason)
                    } else if status.allowed {
                        Msg::CanIResult {
                            generation: genr,
                            text: format!("✓ yes — can {subject}"),
                            ok: true,
                        }
                    } else {
                        deny_msg(genr, &subject, reason)
                    }
                }
                Err(e) => Msg::CanIResult {
                    generation: genr,
                    text: format!("can-i {subject}: review failed: {e}"),
                    ok: false,
                },
            };
            let _ = tx.send(msg).await;
        });
    }
}

fn deny_msg(generation: u64, subject: &str, reason: Option<String>) -> Msg {
    let tail = reason.map(|r| format!(" ({r})")).unwrap_or_default();
    Msg::CanIResult {
        generation,
        text: format!("✗ no — cannot {subject}{tail}"),
        ok: false,
    }
}

/// Format a `SelfSubjectRulesReview` status into the overview document.
fn format_rules(
    status: Option<k8s_openapi::api::authorization::v1::SubjectRulesReviewStatus>,
    ns: &str,
) -> Vec<String> {
    let Some(status) = status else {
        return vec!["no rules returned".into()];
    };
    let mut lines = Vec::new();
    if status.incomplete {
        lines.push(
            "⚠ incomplete — this cluster delegates authorization (e.g. to a cloud IAM),".into(),
        );
        lines.push("  so the rule list below may be partial.".into());
        lines.push(String::new());
    }

    lines.push(format!("Resource permissions in namespace {ns}"));
    if status.resource_rules.is_empty() {
        lines.push("  (none)".into());
    }
    // One line per rule: "VERBS  group/resources[  (resourceNames)]".
    let mut rules: Vec<String> = status
        .resource_rules
        .iter()
        .map(|r| {
            let verbs = join_or_star(&r.verbs);
            let groups = r.api_groups.clone().unwrap_or_default();
            let resources = r.resources.clone().unwrap_or_default();
            let res = resources
                .iter()
                .map(|res| qualify(&groups, res))
                .collect::<Vec<_>>()
                .join(", ");
            let names = r.resource_names.clone().unwrap_or_default();
            let names = if names.is_empty() {
                String::new()
            } else {
                format!("  [names: {}]", names.join(", "))
            };
            format!("  {verbs:<28} {res}{names}")
        })
        .collect();
    rules.sort();
    lines.extend(rules);

    if !status.non_resource_rules.is_empty() {
        lines.push(String::new());
        lines.push("Non-resource URLs".into());
        let mut nr: Vec<String> = status
            .non_resource_rules
            .iter()
            .map(|r| {
                let verbs = join_or_star(&r.verbs);
                let urls = r.non_resource_urls.clone().unwrap_or_default().join(", ");
                format!("  {verbs:<28} {urls}")
            })
            .collect();
        nr.sort();
        lines.extend(nr);
    }

    lines.push(String::new());
    lines.push("Check a specific action with:  :can-i <verb> <resource> [namespace]".into());
    lines
}

/// Join verbs, collapsing a lone `*` to a readable "all".
fn join_or_star(verbs: &[String]) -> String {
    if verbs.iter().any(|v| v == "*") {
        "all".into()
    } else {
        verbs.join(",")
    }
}

/// Qualify a resource with its api group(s) for display (`apps:deployments`).
fn qualify(groups: &[String], resource: &str) -> String {
    let g = groups
        .iter()
        .find(|g| !g.is_empty())
        .cloned()
        .unwrap_or_default();
    if g.is_empty() || g == "*" {
        resource.to_string()
    } else {
        format!("{g}:{resource}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::authorization::v1::{
        NonResourceRule, ResourceRule, SubjectRulesReviewStatus,
    };

    #[test]
    fn formats_resource_and_nonresource_rules() {
        let status = SubjectRulesReviewStatus {
            incomplete: false,
            evaluation_error: None,
            resource_rules: vec![
                ResourceRule {
                    verbs: vec!["get".into(), "list".into(), "watch".into()],
                    api_groups: Some(vec!["apps".into()]),
                    resources: Some(vec!["deployments".into()]),
                    ..Default::default()
                },
                ResourceRule {
                    verbs: vec!["*".into()],
                    api_groups: Some(vec!["".into()]),
                    resources: Some(vec!["pods".into()]),
                    resource_names: Some(vec!["api".into()]),
                },
            ],
            non_resource_rules: vec![NonResourceRule {
                verbs: vec!["get".into()],
                non_resource_urls: Some(vec!["/healthz".into()]),
            }],
        };
        let out = format_rules(Some(status), "prod").join("\n");
        assert!(out.contains("namespace prod"), "{out}");
        assert!(
            out.contains("get,list,watch") && out.contains("apps:deployments"),
            "{out}"
        );
        assert!(
            out.contains("all") && out.contains("pods") && out.contains("[names: api]"),
            "{out}"
        );
        assert!(
            out.contains("Non-resource URLs") && out.contains("/healthz"),
            "{out}"
        );
    }

    #[test]
    fn incomplete_review_is_flagged() {
        let status = SubjectRulesReviewStatus {
            incomplete: true,
            evaluation_error: None,
            resource_rules: vec![],
            non_resource_rules: vec![],
        };
        let out = format_rules(Some(status), "default").join("\n");
        assert!(out.contains("incomplete"), "{out}");
    }
}
