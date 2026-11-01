use std::net::IpAddr;

use http::Uri;
use kube::{Config, config::Kubeconfig};

/// kube-rs parses `NO_PROXY` while it loads a kubeconfig and refuses the
/// whole config over one entry it cannot read, such as the empty entry a
/// trailing comma leaves. Rewrite each variable to the entries kube-rs
/// accepts, and return a warning for each variable that lost a non-empty entry.
///
/// # Safety
///
/// Modifies the process environment, so no other thread may be running.
pub(super) unsafe fn sanitize_env() -> Vec<String> {
    let mut warnings = Vec::new();
    for name in ["NO_PROXY", "no_proxy"] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        let Some((clean, dropped)) = sanitize(&value) else {
            continue;
        };
        if dropped > 0 {
            warnings.push(dropped_warning(name, dropped));
        }
        let unset = clean.is_none();
        // SAFETY: the caller guarantees no other thread is running.
        match clean {
            Some(clean) => unsafe { std::env::set_var(name, clean) },
            None => unsafe { std::env::remove_var(name) },
        }
        // A non-blank NO_PROXY hides no_proxy. When none of its entries are
        // usable, unset both, so no_proxy does not start to apply.
        if name == "NO_PROXY" && unset && !value.trim().is_empty() {
            unsafe { std::env::remove_var("no_proxy") };
            break;
        }
    }
    warnings
}

/// Names the variable and counts the entries, but never quotes them: the
/// environment can hold values that should not reach stderr or a log.
fn dropped_warning(name: &str, dropped: usize) -> String {
    let (entries, are) = if dropped == 1 {
        ("entry", "is")
    } else {
        ("entries", "are")
    };
    format!("{name}: ignoring {dropped} {entries} that {are} not a host, IP address or CIDR range")
}

/// `None` when kube-rs accepts `value` as it is. Otherwise the replacement
/// value (`None` inside to unset the variable) and how many non-empty
/// entries were dropped.
fn sanitize(value: &str) -> Option<(Option<String>, usize)> {
    // kube-rs skips an empty variable and reads a lone `*` as a wildcard.
    if value.is_empty() || value.trim() == "*" {
        return None;
    }
    if value.split(',').map(str::trim).all(kube_accepts) {
        return None;
    }
    let mut kept = Vec::new();
    let mut dropped = 0;
    let mut wildcard = false;
    for entry in value.split(',').map(str::trim) {
        if entry == "*" {
            wildcard = true;
        } else if kube_accepts(entry) {
            kept.push(entry);
        } else if !entry.is_empty() {
            dropped += 1;
        }
    }
    // kube-rs only reads `*` as the whole value, and it excludes everything.
    let clean = if wildcard {
        Some("*".to_owned())
    } else {
        (!kept.is_empty()).then(|| kept.join(","))
    };
    Some((clean, dropped))
}

/// The entries `kube::config` accepts: an IP network, an IP address, or
/// anything that parses as a URI with a host.
fn kube_accepts(entry: &str) -> bool {
    !entry.is_empty()
        && (entry.parse::<ipnet::IpNet>().is_ok()
            || entry.parse::<IpAddr>().is_ok()
            || entry.parse::<Uri>().is_ok_and(|uri| uri.host().is_some()))
}

pub(super) fn configure(config: &mut Config, kubeconfig: &Kubeconfig, context: Option<&str>) {
    let context = context.or(kubeconfig.current_context.as_deref());
    let cluster = kubeconfig
        .contexts
        .iter()
        .find(|entry| Some(entry.name.as_str()) == context)
        .and_then(|entry| entry.context.as_ref())
        .and_then(|context| {
            kubeconfig
                .clusters
                .iter()
                .find(|entry| entry.name == context.cluster)
        })
        .and_then(|entry| entry.cluster.as_ref());
    let Some(cluster) = cluster else { return };
    // kube-rs applies NO_PROXY too, but matches it against the proxy instead
    // of the API server, and lets it drop an explicit proxy-url. Choose the
    // proxy the way kube-rs did before it read NO_PROXY, then match here.
    let explicit = cluster.proxy_url.as_deref().filter(|url| !url.is_empty());
    let proxy = explicit
        .map(str::to_owned)
        .or_else(|| env_nonempty("HTTPS_PROXY"))
        .or_else(|| env_nonempty("https_proxy"));
    if let Some(proxy) = proxy.and_then(|url| url.parse::<Uri>().ok()) {
        config.proxy_url = Some(proxy);
    }
    // An explicit kubeconfig proxy takes priority over environment exclusions.
    if explicit.is_some() {
        return;
    }
    let exclusions = first_nonempty(
        std::env::var("NO_PROXY").ok(),
        std::env::var("no_proxy").ok(),
    );
    if exclusions.is_some_and(|value| matches(&config.cluster_url, &value)) {
        config.proxy_url = None;
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn first_nonempty(upper: Option<String>, lower: Option<String>) -> Option<String> {
    upper
        .filter(|value| !value.trim().is_empty())
        .or(lower.filter(|value| !value.trim().is_empty()))
}

fn matches(server: &Uri, exclusions: &str) -> bool {
    let Some(host) = server.host() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let address = host.parse::<IpAddr>().ok();
    let port = server.port_u16().or_else(|| match server.scheme_str() {
        Some("http") => Some(80),
        Some("https") => Some(443),
        _ => None,
    });
    exclusions
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Ok(network) = entry.parse::<ipnet::IpNet>() {
                return address.is_some_and(|address| network.contains(&address));
            }
            if let Ok(ip) = entry.parse::<IpAddr>() {
                return address == Some(ip);
            }
            let (entry_host, entry_port) = if let Some(rest) = entry.strip_prefix('[') {
                let Some((host, suffix)) = rest.split_once(']') else {
                    return false;
                };
                if suffix.is_empty() {
                    (host, None)
                } else if let Some(port) = suffix.strip_prefix(':') {
                    (host, Some(port))
                } else {
                    return false;
                }
            } else if let Some((host, port)) = entry.split_once(':') {
                (host, Some(port))
            } else {
                (entry, None)
            };
            if let Some(entry_port) = entry_port
                && (entry_port.is_empty() || entry_port.parse::<u16>().ok() != port)
            {
                return false;
            }
            if let Ok(ip) = entry_host.parse::<IpAddr>() {
                return address == Some(ip);
            }
            if address.is_some() {
                return false;
            }
            let entry_host = entry_host.to_ascii_lowercase();
            let domain = if entry_host.starts_with("*.") {
                &entry_host[1..]
            } else {
                &entry_host
            };
            if domain.is_empty() || domain == "." {
                return false;
            }
            if domain.starts_with('.') {
                host.ends_with(domain)
            } else {
                host == domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_hosts_addresses_networks_and_ports() {
        for (server, exclusions, expected) in [
            ("https://rke2-server:6443", "rke2-server", true),
            ("https://rke2-server:6443", " other, RKE2-SERVER , ", true),
            ("https://rke2-server:6443", "other", false),
            ("https://rke2-server:6443", "", false),
            ("https://rke2-server:6443", " , , ", false),
            ("https://api.example.com:6443", "example.com", true),
            ("https://example.com", "example.com", true),
            ("https://notexample.com", "example.com", false),
            ("https://example.com.evil", "example.com", false),
            ("https://api.example.com", ".example.com", true),
            ("https://example.com", ".example.com", false),
            ("https://api.example.com", "*.example.com", true),
            ("https://example.com", "*.example.com", false),
            ("https://api.example.com", "*example.com", false),
            ("https://rke2-server:6443", "rke2-server:6443", true),
            ("https://rke2-server:6443", "rke2-server:443", false),
            ("https://rke2-server", "rke2-server:443", true),
            ("http://rke2-server", "rke2-server:80", true),
            ("https://rke2-server", "rke2-server:", false),
            ("https://rke2-server", "rke2-server:invalid", false),
            ("https://10.20.30.40:6443", "10.20.30.40", true),
            ("https://10.20.30.40:6443", "10.20.30.40:6443", true),
            ("https://10.20.30.40:6443", "10.20.30.40:443", false),
            ("https://10.20.30.40", "10.20.0.0/16", true),
            ("https://10.21.30.40", "10.20.0.0/16", false),
            ("https://10.20.30.40", "10.20.0.0/99", false),
            ("https://10.20.30.40", "30.40", false),
            ("https://rke2-server", "10.20.0.0/16", false),
            ("https://[2001:db8::1]:6443", "2001:db8::1", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]:6443", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]:443", false),
            ("https://[2001:db8::1]", "2001:db8::/32", true),
            ("https://[2001:db9::1]", "2001:db8::/32", false),
            ("https://[2001:db8::1]", "[2001:db8::1", false),
            ("https://[2001:db8::1]", "[2001:db8::1]invalid", false),
            ("https://rke2-server", "*", true),
            ("https://10.20.30.40", "*", true),
            ("https://[2001:db8::1]", "*", true),
            ("https://rke2-server", "https://rke2-server", false),
        ] {
            assert_eq!(
                matches(&server.parse().unwrap(), exclusions),
                expected,
                "{server}, {exclusions}"
            );
        }
    }

    #[test]
    fn sanitize_keeps_values_kube_accepts() {
        for value in [
            "",
            "*",
            " * ",
            "localhost",
            "localhost,127.0.0.1",
            "localhost, 127.0.0.1 ,10.0.0.0/8",
            "example.com,.example.com,*.example.com",
            "rke2-server:6443,10.20.30.40:6443",
            "2001:db8::1,[2001:db8::1],[2001:db8::1]:6443,2001:db8::/32",
            "https://rke2-server",
        ] {
            assert_eq!(sanitize(value), None, "{value:?}");
        }
    }

    #[test]
    fn sanitize_drops_entries_kube_rejects() {
        for (value, clean, dropped) in [
            ("localhost,127.0.0.1,", Some("localhost,127.0.0.1"), 0),
            (",localhost", Some("localhost"), 0),
            ("localhost,,10.0.0.0/8", Some("localhost,10.0.0.0/8"), 0),
            ("<local>,localhost", Some("localhost"), 1),
            ("localhost,a b,<local>", Some("localhost"), 2),
            ("  ", None, 0),
            (" , ,", None, 0),
            ("<local>", None, 1),
            ("localhost,*,", Some("*"), 0),
            ("<local>,*", Some("*"), 1),
            ("*,<local>", Some("*"), 1),
        ] {
            assert_eq!(
                sanitize(value),
                Some((clean.map(str::to_owned), dropped)),
                "{value:?}"
            );
        }
    }

    #[test]
    fn dropped_warning_counts_entries_without_quoting_them() {
        assert_eq!(
            dropped_warning("NO_PROXY", 1),
            "NO_PROXY: ignoring 1 entry that is not a host, IP address or CIDR range"
        );
        assert_eq!(
            dropped_warning("no_proxy", 3),
            "no_proxy: ignoring 3 entries that are not a host, IP address or CIDR range"
        );
    }

    #[test]
    fn sanitized_values_keep_their_exclusions() {
        let (clean, _) = sanitize("<local>,rke2-server,,10.20.0.0/16,").unwrap();
        let clean = clean.unwrap();
        assert!(matches(
            &"https://rke2-server:6443".parse().unwrap(),
            &clean
        ));
        assert!(matches(&"https://10.20.30.40".parse().unwrap(), &clean));
        assert!(!matches(&"https://other".parse().unwrap(), &clean));
    }

    #[test]
    fn uppercase_exclusions_take_priority_unless_empty() {
        for (upper, lower, expected) in [
            (Some("upper"), Some("lower"), Some("upper")),
            (Some(""), Some("lower"), Some("lower")),
            (Some(" \t\n"), Some("lower"), Some("lower")),
            (Some(" upper "), Some("lower"), Some(" upper ")),
            (None, Some("lower"), Some("lower")),
            (Some("upper"), None, Some("upper")),
            (None, None, None),
            (Some(""), Some(""), None),
            (Some(" \t"), Some("\n "), None),
            (None, Some(" \t\n"), None),
        ] {
            assert_eq!(
                first_nonempty(upper.map(str::to_owned), lower.map(str::to_owned)).as_deref(),
                expected
            );
        }
    }
}
