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

/// A compiled log filter. Cheap to query per line; build once when the filter
/// text changes.
pub struct LogMatcher {
    negate: bool,
    kind: Kind,
}

enum Kind {
    /// Empty filter — everything matches.
    All,
    /// Case-insensitive substring (stored lowercased).
    Substr(String),
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
            Kind::Substr(rest.to_lowercase())
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
            Kind::Substr(s) => line.to_lowercase().contains(s),
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
}
