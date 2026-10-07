//! Redaction shared by everything sofka writes out: diagnostic bundles, the
//! `:info` / `sofka info` reports, and the structured log.
//!
//! Two layers, because there are two kinds of output. [`CREDENTIAL_HINTS`] is
//! the key-name vocabulary — what a *field name* has to look like before its
//! value counts as a credential — and is used directly by `bundle` when it
//! walks an object's annotations. [`text`] applies that same vocabulary to
//! free-form strings (a log field, an API server URL, an error message from a
//! library that echoed the request back), where the key and the value arrive
//! together in one line.
//!
//! `text` is deliberately eager: a false positive costs one unreadable value
//! in a log, a false negative leaks a bearer token to disk.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, MatchKind};

/// Placeholder substituted for any redacted value.
pub const REDACTED: &str = "«redacted»";

/// Key-name substrings that mark a value as credential-like. Matched
/// case-insensitively as a substring, so `access_token` and `clientSecret` are
/// covered by `token` and `secret`.
///
/// Substrings only — no bare `key` or `auth`, which would swallow `key=CPU`
/// and every `authorized-*` annotation without protecting anything.
pub const CREDENTIAL_HINTS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "passphrase",
    "secret",
    "apikey",
    "api-key",
    "api_key",
    "credential",
    "private-key",
    "privatekey",
    "authorization",
    "client-key-data",
    "client-certificate-data",
    "certificate-authority-data",
];

/// Extra literals `text` looks for that aren't key names: the two header
/// schemes that carry a credential inline, and the JWT prefix, which is the
/// one token shape common enough to recognise on sight.
const SPECIALS: &[&str] = &["bearer", "basic", "eyj", "://"];

/// Where a value ends when it isn't quoted.
const TERMINATORS: &[u8] = b" \t\r\n,;&\"'}])";

/// Header schemes whose value runs to end of line rather than to the next
/// space (`Authorization: Bearer <jwt>` is one credential, not two tokens).
const SCHEMES: &[&str] = &["bearer", "basic", "negotiate", "digest"];

fn matcher() -> &'static AhoCorasick {
    static MATCHER: OnceLock<AhoCorasick> = OnceLock::new();
    MATCHER.get_or_init(|| {
        let patterns: Vec<&str> = CREDENTIAL_HINTS.iter().chain(SPECIALS).copied().collect();
        AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns)
            .expect("static redaction patterns")
    })
}

/// Whether `key` names a credential (case-insensitive substring match against
/// [`CREDENTIAL_HINTS`]).
///
/// Runs against every annotation key of every object in a diagnostic bundle,
/// so it reuses the shared matcher — one pass, no lowercased copy of the key.
/// [`SPECIALS`] share that matcher and are not key names, hence the index
/// check.
pub fn is_credential_key(key: &str) -> bool {
    matcher()
        .find_iter(key)
        .any(|m| m.pattern().as_usize() < CREDENTIAL_HINTS.len())
}

/// Replace credential-like values and literal IP addresses with [`REDACTED`].
///
/// Recognises `key=value` / `key: value` (quoted or bare) for any key matching
/// [`CREDENTIAL_HINTS`], `Bearer`/`Basic` header values, bare JWTs, and URL
/// userinfo (`https://user:pass@host` → `https://«redacted»@host`).
///
/// Borrows when there is nothing to redact, which is the overwhelmingly common
/// case — this runs on every structured log field.
pub fn text(input: &str) -> Cow<'_, str> {
    match credentials(input) {
        Cow::Borrowed(value) => ip_addresses(value),
        Cow::Owned(value) => match ip_addresses(&value) {
            Cow::Owned(masked) => Cow::Owned(masked),
            Cow::Borrowed(_) => Cow::Owned(value),
        },
    }
}

fn ip_addresses(input: &str) -> Cow<'_, str> {
    static CANDIDATES: OnceLock<regex::Regex> = OnceLock::new();
    let candidates = CANDIDATES
        .get_or_init(|| regex::Regex::new(r"[0-9A-Fa-f:.]+").expect("static IP address pattern"));
    let mut out: Option<String> = None;
    let mut last = 0;
    for candidate in candidates.find_iter(input) {
        let mut start = candidate.start();
        let value = candidate.as_str().trim_end_matches('.');
        let value = if value.starts_with(':') && !value.starts_with("::") {
            start += 1;
            &value[1..]
        } else {
            value
        };
        let length = if value.parse::<IpAddr>().is_ok() {
            value.len()
        } else if value.parse::<SocketAddr>().is_ok() {
            value.rfind(':').expect("socket address has a port")
        } else {
            continue;
        };
        let end = start + length;
        // Keep version strings and names that contain address-like text.
        if input[..start].chars().next_back().is_some_and(ip_name_char)
            || input[candidate.end()..]
                .chars()
                .next()
                .is_some_and(ip_name_char)
        {
            continue;
        }
        let output = out.get_or_insert_with(|| String::with_capacity(input.len()));
        output.push_str(&input[last..start]);
        output.push_str(REDACTED);
        last = end;
    }
    match out {
        Some(mut output) => {
            output.push_str(&input[last..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(input),
    }
}

fn ip_name_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

/// [`text`] without the IP address pass: only credential-like values. For
/// text that has to stay usable, such as a command the user is told to run.
pub fn credentials(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut out: Option<String> = None;
    let mut last = 0usize;

    for m in matcher().find_iter(input) {
        // A previous value already swallowed this match (`token=Bearer x`).
        if m.start() < last {
            continue;
        }
        let span = if m.pattern().as_usize() < CREDENTIAL_HINTS.len() {
            key_value_span(bytes, m.end())
        } else {
            match SPECIALS[m.pattern().as_usize() - CREDENTIAL_HINTS.len()] {
                "://" => userinfo_span(bytes, m.end()),
                // A JWT loose in an error message, with no key naming it.
                "eyj" => jwt_span(bytes, m.start()),
                // `Bearer <token>` reached without a key in front of it.
                _ => scheme_span(bytes, m.end()),
            }
        };
        let Some((start, end)) = span else { continue };
        let out = out.get_or_insert_with(|| String::with_capacity(input.len()));
        out.push_str(&input[last..start]);
        out.push_str(REDACTED);
        last = end;
    }

    match out {
        Some(mut out) => {
            out.push_str(&input[last..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(input),
    }
}

/// The value span following a credential key that ends at `after_key`, or
/// `None` when the key isn't actually introducing a value (`kind=secrets`,
/// the word "password" in prose).
fn key_value_span(bytes: &[u8], after_key: usize) -> Option<(usize, usize)> {
    let mut i = after_key;
    while bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        i += 1;
    }
    // The rest of a quoted key ("client-secret": …) and any padding before
    // the separator, but never a newline: a key at end of line has no value.
    while matches!(bytes.get(i), Some(b'"' | b'\'' | b' ' | b'\t')) {
        i += 1;
    }
    if !matches!(bytes.get(i), Some(b'=' | b':')) {
        return None;
    }
    i += 1;
    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    match bytes.get(i) {
        Some(&q @ (b'"' | b'\'')) => {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end = (end + 2).min(bytes.len()),
                    c if c == q => break,
                    _ => end += 1,
                }
            }
            (end > start).then_some((start, end))
        }
        Some(_) => {
            let start = i;
            // `Authorization: Bearer <jwt>` — the scheme word is part of the
            // credential's shape, so the whole rest of the line goes.
            if let Some(span) = scheme_here(bytes, start) {
                return Some(span);
            }
            let end = bytes[start..]
                .iter()
                .position(|b| TERMINATORS.contains(b))
                .map_or(bytes.len(), |n| start + n);
            (end > start).then_some((start, end))
        }
        None => None,
    }
}

/// Span of the JWT starting at `at` — base64url segments and their dots.
fn jwt_span(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let end = bytes[at..]
        .iter()
        .position(|b| {
            !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/' | b'='))
        })
        .map_or(bytes.len(), |n| at + n);
    (end > at).then_some((at, end))
}

/// Span of a `Bearer`/`Basic` credential whose scheme word ends at `after`.
fn scheme_span(bytes: &[u8], after: usize) -> Option<(usize, usize)> {
    let mut i = after;
    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    let start = i;
    let end = line_end(bytes, start);
    (end > start).then_some((start, end))
}

/// Whether a header scheme word starts at `at` and, if so, the span of the
/// credential after it. The scheme name itself stays: `Bearer «redacted»` says
/// which auth path the request took, which is exactly what a log is for.
fn scheme_here(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let rest = &bytes[at..];
    for scheme in SCHEMES {
        let n = scheme.len();
        if rest.len() > n
            && rest[..n].eq_ignore_ascii_case(scheme.as_bytes())
            && matches!(rest[n], b' ' | b'\t')
        {
            return scheme_span(bytes, at + n);
        }
    }
    None
}

/// Userinfo between a `://` ending at `after` and the authority's `@`.
fn userinfo_span(bytes: &[u8], after: usize) -> Option<(usize, usize)> {
    let end = bytes[after..]
        .iter()
        .position(|b| matches!(b, b'@' | b'/' | b'?' | b'#' | b' ' | b'\t' | b'\r' | b'\n'))
        .map(|n| after + n)?;
    (bytes[end] == b'@' && end > after).then_some((after, end))
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    memchr::memchr2(b'\n', b'\r', &bytes[from..]).map_or(bytes.len(), |n| from + n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn red(input: &str) -> String {
        text(input).into_owned()
    }

    #[test]
    fn clean_text_is_borrowed() {
        assert!(matches!(
            text("context=prod cluster=eu-west kinds=142"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn redacts_ip_addresses_and_keeps_ports_and_url_paths() {
        for (input, expected) in [
            (
                "https://10.40.0.3:6443/api",
                format!("https://{REDACTED}:6443/api"),
            ),
            (
                "https://[fd00::3]:6443/api",
                format!("https://[{REDACTED}]:6443/api"),
            ),
            (
                "dial 192.0.2.1:443 failed",
                format!("dial {REDACTED}:443 failed"),
            ),
            ("peer:10.40.0.3", format!("peer:{REDACTED}")),
            (
                "address 2001:db8::1 rejected",
                format!("address {REDACTED} rejected"),
            ),
            (
                "address ::ffff:192.0.2.1 rejected",
                format!("address {REDACTED} rejected"),
            ),
            (
                "https://[fe80::1%25en0]:6443",
                format!("https://[{REDACTED}%25en0]:6443"),
            ),
            ("peer 192.0.2.1.", format!("peer {REDACTED}.")),
            (
                "192.0.2.1 and 198.51.100.2",
                format!("{REDACTED} and {REDACTED}"),
            ),
        ] {
            assert_eq!(red(input), expected, "{input}");
        }
        assert_eq!(
            red("https://admin:pw@10.40.0.3:6443/?token_value=private"),
            format!("https://{REDACTED}@{REDACTED}:6443/?token_value={REDACTED}")
        );
    }

    #[test]
    fn keeps_versions_timestamps_hostnames_and_invalid_addresses() {
        for value in [
            "v1.37.0",
            "v1.2.3.4",
            "2026-09-07T12:34:56.789Z",
            "https://api.example.com:6443",
            "999.1.2.3",
            "api.192.0.2.1.example",
        ] {
            assert!(matches!(text(value), Cow::Borrowed(_)), "{value}");
        }
    }

    #[test]
    fn redacts_key_value_credentials() {
        assert_eq!(
            red("token=abc123 next=1"),
            format!("token={REDACTED} next=1")
        );
        assert_eq!(red("password: hunter2"), format!("password: {REDACTED}"));
        assert_eq!(
            red(r#"{"client-secret": "s3cr3t", "user": "ana"}"#),
            format!(r#"{{"client-secret": "{REDACTED}", "user": "ana"}}"#)
        );
    }

    #[test]
    fn redacts_credential_keys_with_suffixes() {
        for key in [
            "token_value",
            "passwordText",
            "client-secret.data",
            "API_KEY_1",
        ] {
            assert_eq!(
                red(&format!("{key}=private123 next=1")),
                format!("{key}={REDACTED} next=1")
            );
        }
        assert_eq!(
            red("https://api.example/?token_value=private123&limit=5"),
            format!("https://api.example/?token_value={REDACTED}&limit=5")
        );
    }

    #[test]
    fn redacts_escaped_quotes_and_keeps_the_next_field() {
        for secret in [
            r#"abc\"private123"#,
            r"abc\\",
            r#"abc\\\"private123"#,
            "пароль",
        ] {
            let input = format!(r#"{{"password":"{secret}","user":"ana"}}"#);
            let output = red(&input);
            assert_eq!(
                output,
                format!(r#"{{"password":"{REDACTED}","user":"ana"}}"#)
            );
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(parsed["user"], "ana");
        }
        assert_eq!(
            red(r"password='abc\'private123' next=1"),
            format!("password='{REDACTED}' next=1")
        );
        assert_eq!(
            red(r#"password="abc\"private123"#),
            format!("password=\"{REDACTED}")
        );
    }

    #[test]
    fn redacts_kubeconfig_credential_fields() {
        for key in [
            "client-key-data",
            "client-certificate-data",
            "certificate-authority-data",
        ] {
            assert_eq!(
                red(&format!("{key}: LS0tLS1CRUdJTg==")),
                format!("{key}: {REDACTED}")
            );
        }
    }

    #[test]
    fn redacts_bearer_header_to_end_of_line() {
        // The scheme name survives; everything after it does not, so a token
        // with a space in it cannot slip through on the tail.
        assert_eq!(
            red("authorization: Bearer eyJhbGciOi.J9.sig"),
            format!("authorization: Bearer {REDACTED}")
        );
        assert_eq!(
            red("sent Bearer eyJhbGciOi with 3 retries\nnext line"),
            format!("sent Bearer {REDACTED}\nnext line")
        );
        assert_eq!(
            red("Authorization: Basic dXNlcjpwdw=="),
            format!("Authorization: Basic {REDACTED}")
        );
    }

    #[test]
    fn redacts_bare_jwt() {
        assert_eq!(
            red("stream failed: eyJhbGciOiJSUzI1NiJ9.e30.sig rejected"),
            format!("stream failed: {REDACTED} rejected")
        );
    }

    #[test]
    fn redacts_url_userinfo_only() {
        assert_eq!(
            red("https://admin:hunter2@api.example.com:6443/version"),
            format!("https://{REDACTED}@api.example.com:6443/version")
        );
        assert_eq!(
            red("https://api.example.com:6443/version"),
            "https://api.example.com:6443/version"
        );
    }

    #[test]
    fn keeps_resource_plurals_and_prose() {
        // The one false positive that would matter: `secrets` is a kind name
        // that shows up in discovery output and every log line about them.
        assert_eq!(
            red("event=watch.start kind=secrets ns=default"),
            "event=watch.start kind=secrets ns=default"
        );
        assert_eq!(red("kind=secret ns=default"), "kind=secret ns=default");
        assert_eq!(
            red("listing secrets is forbidden"),
            "listing secrets is forbidden"
        );
    }

    #[test]
    fn empty_values_are_left_alone() {
        assert_eq!(red("token="), "token=");
        assert_eq!(red("password"), "password");
    }

    #[test]
    fn redacts_every_occurrence() {
        assert_eq!(
            red("token=a password=b user=c apikey=d"),
            format!("token={REDACTED} password={REDACTED} user=c apikey={REDACTED}")
        );
    }

    #[test]
    fn is_credential_key_matches_case_insensitively() {
        assert!(is_credential_key("clientSecret"));
        assert!(is_credential_key("X-API-Key"));
        assert!(!is_credential_key("kubectl.kubernetes.io/restartedAt"));
    }
}
