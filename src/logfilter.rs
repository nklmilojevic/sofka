//! Log-view filter matching: substring, regex, and inverse.
//!
//! The `/` filter in the logs view accepts three forms, so a busy stream can be
//! narrowed the way you'd expect from `grep`:
//!
//! - `text`        — case-insensitive substring (the default)
//! - `/re/`        — a regular expression (case-insensitive)
//! - `!text`,`!/re/` — inverse: keep the lines that *don't* match
//!
//! An empty filter matches everything. A malformed regex matches nothing and is
//! flagged via [`LogMatcher::is_error`] so the view can say so instead of
//! silently hiding the whole buffer.

/// Severity detected with the same format rules as log colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Debug,
    Other,
}

pub fn is_warning_or_error(line: &str) -> bool {
    matches!(severity(line), Severity::Warning | Severity::Error)
}

pub fn severity(line: &str) -> Severity {
    let line = crate::ui::strip_ansi_if_present(line);
    let l = line.to_ascii_lowercase();
    // Structured logs: read the level field directly (authoritative; a later
    // "…error…" in the message can't override it).
    if let Some(level) = json_field(&l, "level").or_else(|| json_field(&l, "severity")) {
        return parse_level(level);
    }
    // Skip source labels and timestamps before checking the klog level.
    let body = log_body(&line);
    if klog_level(body, 'e') || klog_level(body, 'f') {
        return Severity::Error;
    }
    if klog_level(body, 'w') {
        return Severity::Warning;
    }
    // Otherwise the leftmost level marker wins, since the level precedes the
    // message; so a later "…the last error:" can't override a `warn` level.
    let first = |needles: &[&str]| needles.iter().filter_map(|n| l.find(n)).min();
    let candidates = [
        (
            first(&[
                " error",
                "\terror",
                "zerror",
                "level=error",
                " fatal",
                "zfatal",
                " panic",
            ]),
            Severity::Error,
        ),
        (
            first(&[" warn", "\twarn", "zwarn", "level=warn"]),
            Severity::Warning,
        ),
        (
            first(&[
                " debug",
                "\tdebug",
                "zdebug",
                " trace",
                "ztrace",
                "level=debug",
            ]),
            Severity::Debug,
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(pos, color)| pos.map(|p| (p, color)))
        .min_by_key(|(p, _)| *p)
        .map(|(_, color)| color)
        .unwrap_or(Severity::Other)
}

/// Severity for a level token in lowercase.
pub(crate) fn parse_level(level: &str) -> Severity {
    if level.starts_with("err")
        || level.starts_with("fatal")
        || level.starts_with("crit")
        || level.starts_with("panic")
    {
        Severity::Error
    } else if level.starts_with("warn") {
        Severity::Warning
    } else if level.starts_with("debug") || level.starts_with("trace") {
        Severity::Debug
    } else {
        Severity::Other
    }
}

/// Read a JSON string field's value, e.g. `json_field(r#"…"level":"warn"…"#,
/// "level") == Some("warn")`. Tolerant of whitespace around the colon. Input is
/// expected already lowercased.
fn json_field<'a>(l: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let i = l.find(&pat)?;
    let rest = l[i + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn log_body(mut line: &str) -> &str {
    if line.starts_with('[')
        && let Some((_, rest)) = line.split_once("] ")
    {
        line = rest;
    }
    if let Some((timestamp, rest)) = line.split_once(' ')
        && timestamp.parse::<k8s_openapi::jiff::Timestamp>().is_ok()
    {
        line = rest;
    }
    line
}

/// Check for a klog level marker at the start of the message.
fn klog_level(line: &str, level: char) -> bool {
    let mut it = line.chars();
    it.next().is_some_and(|c| c.eq_ignore_ascii_case(&level))
        && it.next().is_some_and(|c| c.is_ascii_digit())
}

/// A compiled log filter. Cheap to query per line; build once when the filter
/// text changes.
pub struct LogMatcher {
    negate: bool,
    kind: Kind,
}

/// A compiled case-insensitive substring test.
///
/// Built once per filter change and queried per line. Also used on its own by
/// the document search (`/` in a YAML/describe/events view), which wants plain
/// substring semantics without the `!`/`/re/` grammar [`LogMatcher`] adds, and
/// by the row filter's `"quoted"` terms.
#[derive(Clone)]
pub enum Substring {
    /// ASCII pattern, matched by an Aho-Corasick automaton. For a single
    /// pattern the crate picks its `memmem`/Teddy prefilter, which is
    /// SSE2/AVX2 on x86-64 and NEON on aarch64 with a scalar fallback — chosen
    /// at runtime, so one binary stays correct on every release target.
    /// Replaces a hand-rolled sliding-window compare that re-scanned every
    /// byte offset of every line, every frame.
    Ascii(Box<aho_corasick::AhoCorasick>),
    /// Pattern whose *lowercased* form is not ASCII, so matching needs full
    /// Unicode case folding. Stored lowercased.
    Unicode(String),
}

impl Substring {
    /// Compile `pat` (which must not be empty).
    pub fn new(pat: &str) -> Self {
        // Lowercase first, then decide: Unicode lowercasing can land on ASCII
        // (the Kelvin sign U+212A lowercases to plain `k`), so the test has to
        // be on the folded form, not the input.
        let folded = pat.to_lowercase();
        if folded.is_ascii() {
            match aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([folded.as_bytes()])
            {
                Ok(ac) => return Substring::Ascii(Box::new(ac)),
                // Construction only fails on pathological pattern sets; a
                // single literal can't hit that, but fall back rather than
                // panic on a user-supplied filter.
                Err(_) => return Substring::Unicode(folded),
            }
        }
        Substring::Unicode(folded)
    }

    pub fn matches(&self, line: &str) -> bool {
        match self {
            // ASCII bytes can't occur inside a multi-byte UTF-8 sequence, so
            // searching raw bytes is exact on any line — no allocation, and no
            // UTF-8 boundary check needed.
            Substring::Ascii(ac) => ac.is_match(line.as_bytes()),
            Substring::Unicode(s) => {
                // A folded pattern containing a non-ASCII character cannot
                // occur in a pure-ASCII line (lowercasing ASCII yields ASCII).
                // Rejecting those without folding keeps the common case
                // allocation-free — this arm used to lowercase every line in
                // the buffer on every frame.
                !line.is_ascii() && line.to_lowercase().contains(s)
            }
        }
    }
}

enum Kind {
    /// Empty filter — everything matches.
    All,
    Substr(Substring),
    Regex(regex::Regex),
    /// A `/…/` that failed to compile.
    BadRegex,
}

impl Default for LogMatcher {
    fn default() -> Self {
        LogMatcher {
            negate: false,
            kind: Kind::All,
        }
    }
}

impl LogMatcher {
    /// Compile `input` into a matcher. Never fails — a bad regex becomes a
    /// [`Kind::BadRegex`] that matches nothing.
    pub fn new(input: &str) -> Self {
        let (negate, rest) = match input.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, input),
        };
        if rest.is_empty() {
            // `` → All; `!` alone → negate All (matches nothing), which reads
            // as "hide everything", a reasonable literal interpretation.
            return LogMatcher {
                negate,
                kind: Kind::All,
            };
        }
        // `/pattern/` (at least the two slashes) is a regex.
        let kind = if rest.len() >= 2 && rest.starts_with('/') && rest.ends_with('/') {
            let pattern = &rest[1..rest.len() - 1];
            match regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
            {
                Ok(re) => Kind::Regex(re),
                Err(_) => Kind::BadRegex,
            }
        } else {
            Kind::Substr(Substring::new(rest))
        };
        LogMatcher { negate, kind }
    }

    /// Whether `line` passes the filter.
    pub fn matches(&self, line: &str) -> bool {
        // A broken regex hides everything (and `is_error` lets the UI explain),
        // regardless of negation — negating a typo shouldn't reveal the buffer.
        if matches!(self.kind, Kind::BadRegex) {
            return false;
        }
        let base = match &self.kind {
            Kind::All => true,
            Kind::Substr(s) => s.matches(line),
            Kind::Regex(re) => re.is_match(line),
            Kind::BadRegex => false,
        };
        base ^ self.negate
    }

    /// True when the filter is a `/…/` regex that didn't compile.
    pub fn is_error(&self) -> bool {
        matches!(self.kind, Kind::BadRegex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_matches_everything() {
        let m = LogMatcher::new("");
        assert!(m.matches("anything"));
        assert!(!m.is_error());
    }

    #[test]
    fn substring_is_case_insensitive() {
        let m = LogMatcher::new("Error");
        assert!(m.matches("an ERROR happened"));
        assert!(!m.matches("all good"));
    }

    #[test]
    fn inverse_substring_hides_matches() {
        let m = LogMatcher::new("!health");
        assert!(!m.matches("GET /healthz 200"));
        assert!(m.matches("GET /api 500"));
    }

    #[test]
    fn regex_matches_and_is_case_insensitive() {
        let m = LogMatcher::new("/level=(warn|error)/");
        assert!(m.matches("ts=1 level=ERROR msg=boom"));
        assert!(!m.matches("ts=1 level=info msg=ok"));
    }

    #[test]
    fn inverse_regex() {
        let m = LogMatcher::new("!/2\\d\\d/");
        assert!(!m.matches("status 200"));
        assert!(m.matches("status 503"));
    }

    #[test]
    fn bad_regex_matches_nothing_and_flags_error() {
        let m = LogMatcher::new("/[unclosed/");
        assert!(m.is_error());
        assert!(!m.matches("anything"));
        // Even negated, a broken regex hides everything.
        let n = LogMatcher::new("!/[unclosed/");
        assert!(!n.matches("anything"));
    }

    #[test]
    fn slashes_need_both_ends_to_be_a_regex() {
        // A single leading slash is a literal substring, not a regex.
        let m = LogMatcher::new("/api");
        assert!(m.matches("GET /api/v1"));
        assert!(!m.is_error());
    }

    #[test]
    fn substring_matches_at_the_very_start_and_end_of_a_line() {
        let m = LogMatcher::new("abc");
        assert!(m.matches("abc trailing"));
        assert!(m.matches("leading abc"));
        assert!(m.matches("abc"));
        assert!(!m.matches("ab"));
    }

    #[test]
    fn pattern_longer_than_the_line_does_not_match() {
        let m = LogMatcher::new("a-very-long-pattern");
        assert!(!m.matches("short"));
        assert!(!m.matches(""));
    }

    #[test]
    fn ascii_pattern_is_exact_against_multibyte_lines() {
        // An ASCII byte can never occur inside a multi-byte UTF-8 sequence,
        // so a byte search must not produce a false positive mid-character.
        let m = LogMatcher::new("temp");
        assert!(m.matches("日本語 temp 測定"));
        assert!(!m.matches("日本語のログ行"));
    }

    #[test]
    fn non_ascii_pattern_folds_with_unicode_rules() {
        let m = LogMatcher::new("ÜBER");
        assert!(m.matches("etwas über den wolken"));
        assert!(!m.matches("nothing relevant"));
        // A non-ASCII pattern can never occur in a pure-ASCII line; the fast
        // reject for that case must not change the answer.
        assert!(!m.matches("plain ascii line"));
    }

    #[test]
    fn non_ascii_pattern_inverted() {
        let m = LogMatcher::new("!über");
        assert!(!m.matches("etwas ÜBER den wolken"));
        assert!(m.matches("plain ascii line"));
    }

    /// Unicode lowercasing can map a non-ASCII character onto ASCII — the
    /// Kelvin sign (U+212A) folds to a plain `k`. The ASCII/Unicode split is
    /// therefore decided on the *folded* pattern, not the raw input.
    #[test]
    fn pattern_that_folds_from_non_ascii_to_ascii_still_matches_ascii_lines() {
        let m = LogMatcher::new("\u{212A}elvin");
        assert!(m.matches("temperature in kelvin"));
        assert!(m.matches("Temperature in KELVIN"));
    }
}
