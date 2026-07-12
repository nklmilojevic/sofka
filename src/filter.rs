//! Structured row-filter grammar.
//!
//! Plain text with no structured markers stays exactly what it always was:
//! one fuzzy pattern over "namespace name". Once any structured marker
//! appears, the input is split on whitespace and every term must match
//! (terms are AND-ed; there is no OR/grouping — deliberately small):
//!
//! - `text`                   fuzzy match (namespace + name)
//! - `!text`                  inverse fuzzy match
//! - `-l app=api,env=prod`    Kubernetes label selector (sent server-side)
//! - `-f spec.nodeName=n1`    Kubernetes field selector (sent server-side)
//! - `status=CrashLoopBackOff` column equality (case-insensitive)
//! - `cpu>500m` `memory>1Gi` `restarts>=5` `age<2h` typed comparisons
//!
//! Comparison operators: `=` (or `==`), `!=`, `>`, `>=`, `<`, `<=`. The
//! value's type follows the key: `cpu` parses CPU quantities (millicores),
//! `mem`/`memory` memory quantities (bytes), `age` durations (`90s`, `2h`,
//! `1d2h`); any other key compares numerically when the value is a number
//! and as case-insensitive text otherwise. Parsing never fails hard — a
//! broken term is skipped and reported via [`Structured::error`] so the
//! table doesn't blank out mid-keystroke.

/// The parsed form of the filter input.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedFilter {
    /// The whole input is one fuzzy pattern (no structured markers) — the
    /// original `/text` behavior, kept byte-for-byte compatible.
    Fuzzy(String),
    Structured(Structured),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Structured {
    /// Locally-evaluated terms, AND-ed together.
    pub terms: Vec<Term>,
    /// Combined `-l` selectors, ready for the Kubernetes API.
    pub labels: Option<String>,
    /// Combined `-f` selectors, ready for the Kubernetes API.
    pub fields: Option<String>,
    /// First malformed term, for surfacing in the UI.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    Fuzzy(String),
    NotFuzzy(String),
    Cmp(Cmp),
}

/// One `key<op>value` column comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmp {
    /// Lowercased column key (`status`, `cpu`, `restarts`, …).
    pub key: String,
    pub op: Op,
    pub value: CmpValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    /// Apply the operator to an already-computed `actual.cmp(&wanted)`.
    pub fn eval(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Op::Eq => ord == Equal,
            Op::Ne => ord != Equal,
            Op::Gt => ord == Greater,
            Op::Ge => ord != Less,
            Op::Lt => ord == Less,
            Op::Le => ord != Greater,
        }
    }
}

/// A comparison value, typed at parse time from the key it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum CmpValue {
    /// Plain number (`restarts>=5`).
    Num(f64),
    /// CPU quantity in millicores (`cpu>500m`).
    Cpu(i64),
    /// Memory quantity in bytes (`memory>1Gi`).
    Mem(i64),
    /// Duration in seconds (`age<2h`).
    Duration(i64),
    /// Anything else: case-insensitive text comparison.
    Str(String),
}

impl ParsedFilter {
    pub fn labels(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.labels.as_deref(),
        }
    }

    pub fn fields(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.fields.as_deref(),
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.error.as_deref(),
        }
    }

    /// The pattern NAME-cell highlighting should mark: the legacy fuzzy
    /// pattern, or the first positive fuzzy term of a structured filter.
    pub fn fuzzy_needle(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(pat) => (!pat.is_empty()).then_some(pat.as_str()),
            ParsedFilter::Structured(s) => s.terms.iter().find_map(|t| match t {
                Term::Fuzzy(pat) => Some(pat.as_str()),
                _ => None,
            }),
        }
    }
}

pub fn parse(input: &str) -> ParsedFilter {
    let trimmed = input.trim();
    if trimmed.is_empty() || !is_structured(trimmed) {
        return ParsedFilter::Fuzzy(trimmed.to_string());
    }

    let mut terms = Vec::new();
    let mut labels: Vec<&str> = Vec::new();
    let mut fields: Vec<&str> = Vec::new();
    let mut error: Option<String> = None;
    let fail = |slot: &mut Option<String>, msg: String| {
        if slot.is_none() {
            *slot = Some(msg);
        }
    };

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        i += 1;
        // `-l <sel>` / `-f <sel>`, or attached (`-lapp=api`).
        if tok == "-l" || tok == "-f" {
            match tokens.get(i) {
                Some(sel) => {
                    if tok == "-l" {
                        &mut labels
                    } else {
                        &mut fields
                    }
                    .push(sel);
                    i += 1;
                }
                None => fail(&mut error, format!("expected selector after {tok}")),
            }
            continue;
        }
        if let Some(sel) = attached_selector(tok, "-l") {
            labels.push(sel);
            continue;
        }
        if let Some(sel) = attached_selector(tok, "-f") {
            fields.push(sel);
            continue;
        }
        if let Some((key, op, value)) = split_cmp(tok) {
            if value.is_empty() {
                fail(&mut error, format!("missing value in '{tok}'"));
                continue;
            }
            match typed_value(key, value) {
                Ok(v) => terms.push(Term::Cmp(Cmp {
                    key: key.to_ascii_lowercase(),
                    op,
                    value: v,
                })),
                Err(e) => fail(&mut error, e),
            }
            continue;
        }
        if let Some(pat) = tok.strip_prefix('!') {
            if pat.is_empty() {
                fail(&mut error, "expected text after '!'".into());
            } else {
                terms.push(Term::NotFuzzy(pat.to_string()));
            }
            continue;
        }
        terms.push(Term::Fuzzy(tok.to_string()));
    }

    ParsedFilter::Structured(Structured {
        terms,
        labels: (!labels.is_empty()).then(|| labels.join(",")),
        fields: (!fields.is_empty()).then(|| fields.join(",")),
        error,
    })
}

/// Whether any token flips the input from a single legacy fuzzy pattern into
/// the structured grammar. Mirrors the markers `parse` acts on.
fn is_structured(input: &str) -> bool {
    input.split_whitespace().any(|tok| {
        tok == "-l"
            || tok == "-f"
            || attached_selector(tok, "-l").is_some()
            || attached_selector(tok, "-f").is_some()
            || tok.starts_with('!')
            || split_cmp(tok).is_some()
    })
}

/// The selector of an attached `-l`/`-f` form (`-lapp=api`). Requires an `=`
/// so ordinary fuzzy text starting with those letters isn't swallowed.
fn attached_selector<'a>(tok: &'a str, flag: &str) -> Option<&'a str> {
    tok.strip_prefix(flag).filter(|rest| rest.contains('='))
}

/// Split `key<op>value` at the operator following a valid key. `None` when
/// the token has no operator or no leading key — i.e. plain fuzzy text.
fn split_cmp(tok: &str) -> Option<(&str, Op, &str)> {
    if !tok
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let key_end = tok.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
    let (key, rest) = tok.split_at(key_end);
    let (op, value) = if let Some(v) = rest.strip_prefix("!=") {
        (Op::Ne, v)
    } else if let Some(v) = rest.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = rest.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = rest.strip_prefix("==") {
        (Op::Eq, v)
    } else if let Some(v) = rest.strip_prefix('=') {
        (Op::Eq, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (Op::Gt, v)
    } else {
        (Op::Lt, rest.strip_prefix('<')?)
    };
    Some((key, op, value))
}

/// Type a comparison value from its key: quantities for `cpu`/`mem`/`memory`,
/// durations for `age`, and number-or-text for everything else.
fn typed_value(key: &str, raw: &str) -> Result<CmpValue, String> {
    match key.to_ascii_lowercase().as_str() {
        "cpu" => parse_cpu(raw)
            .map(CmpValue::Cpu)
            .ok_or_else(|| format!("bad cpu quantity '{raw}'")),
        "mem" | "memory" => parse_mem(raw)
            .map(CmpValue::Mem)
            .ok_or_else(|| format!("bad memory quantity '{raw}'")),
        "age" => parse_duration(raw)
            .map(CmpValue::Duration)
            .ok_or_else(|| format!("bad duration '{raw}'")),
        _ => Ok(raw
            .parse::<f64>()
            .map(CmpValue::Num)
            .unwrap_or_else(|_| CmpValue::Str(raw.to_string()))),
    }
}

/// CPU quantity → millicores: `250m` → 250, `1` → 1000, `500000000n` → 500.
/// Unlike [`crate::columns::parse_cpu_milli`] this rejects garbage instead of
/// defaulting to 0, so a typo can be reported.
fn parse_cpu(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, scale) = match s.chars().last()? {
        'n' => (&s[..s.len() - 1], 1.0 / 1_000_000.0),
        'u' => (&s[..s.len() - 1], 1.0 / 1_000.0),
        'm' => (&s[..s.len() - 1], 1.0),
        _ => (s, 1000.0),
    };
    let v: f64 = num.parse().ok()?;
    (v >= 0.0).then(|| (v * scale).round() as i64)
}

/// Memory quantity → bytes: `1Gi`, `512Mi`, `2000000`. Validating twin of
/// [`crate::columns::parse_mem_bytes`].
fn parse_mem(s: &str) -> Option<i64> {
    let s = s.trim();
    let suffixes: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0f64.powi(4)),
        ("K", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    for (suf, mult) in suffixes {
        if let Some(num) = s.strip_suffix(suf) {
            let v: f64 = num.trim().parse().ok()?;
            return (v >= 0.0).then(|| (v * mult) as i64);
        }
    }
    let v: f64 = s.parse().ok()?;
    (v >= 0.0).then_some(v as i64)
}

/// Duration → seconds: `90s`, `2h`, `1d2h`, `1h30m`, bare `300` (seconds).
/// Units: s, m, h, d, w.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        return (v >= 0).then_some(v);
    }
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return None,
        };
        if num.is_empty() {
            return None;
        }
        total += num.parse::<i64>().ok()? * unit;
        num.clear();
    }
    // Trailing digits without a unit (`2h30`) are malformed.
    num.is_empty().then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structured(input: &str) -> Structured {
        match parse(input) {
            ParsedFilter::Structured(s) => s,
            other => panic!("expected structured parse for '{input}', got {other:?}"),
        }
    }

    #[test]
    fn plain_text_stays_one_legacy_fuzzy_pattern() {
        assert_eq!(parse(""), ParsedFilter::Fuzzy(String::new()));
        assert_eq!(parse("api"), ParsedFilter::Fuzzy("api".into()));
        // Spaces included: the whole string is the pattern, as before.
        assert_eq!(
            parse("kube system dns"),
            ParsedFilter::Fuzzy("kube system dns".into())
        );
        // Leading/trailing whitespace is not part of the pattern.
        assert_eq!(parse("  api "), ParsedFilter::Fuzzy("api".into()));
        // A lone dash or dashed name is still fuzzy text, not a flag.
        assert_eq!(parse("-longname"), ParsedFilter::Fuzzy("-longname".into()));
    }

    #[test]
    fn inverse_term() {
        let s = structured("!canary");
        assert_eq!(s.terms, vec![Term::NotFuzzy("canary".into())]);
        assert_eq!(s.error, None);
    }

    #[test]
    fn label_selector_variants() {
        let s = structured("-l app=api,env=prod");
        assert_eq!(s.labels.as_deref(), Some("app=api,env=prod"));
        assert!(s.terms.is_empty());
        assert_eq!(s.error, None);

        // Attached form and repeated flags joining with a comma.
        let s = structured("-lapp=api -l env=prod");
        assert_eq!(s.labels.as_deref(), Some("app=api,env=prod"));

        // Bare-key (existence) selectors work in the spaced form.
        let s = structured("-l app");
        assert_eq!(s.labels.as_deref(), Some("app"));
    }

    #[test]
    fn field_selector() {
        let s = structured("-f spec.nodeName=node-3");
        assert_eq!(s.fields.as_deref(), Some("spec.nodeName=node-3"));
        assert_eq!(s.labels, None);
        assert!(s.terms.is_empty());
    }

    #[test]
    fn status_equality_and_inequality() {
        let s = structured("status=CrashLoopBackOff");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "status".into(),
                op: Op::Eq,
                value: CmpValue::Str("CrashLoopBackOff".into()),
            })]
        );

        let s = structured("status!=Running");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "status".into(),
                op: Op::Ne,
                value: CmpValue::Str("Running".into()),
            })]
        );
    }

    #[test]
    fn typed_quantity_comparisons() {
        let s = structured("cpu>500m");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "cpu".into(),
                op: Op::Gt,
                value: CmpValue::Cpu(500),
            })]
        );

        let s = structured("cpu>=1");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "cpu".into(),
                op: Op::Ge,
                value: CmpValue::Cpu(1000),
            })]
        );

        let s = structured("memory>1Gi");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "memory".into(),
                op: Op::Gt,
                value: CmpValue::Mem(1024 * 1024 * 1024),
            })]
        );

        let s = structured("mem<=512Mi");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "mem".into(),
                op: Op::Le,
                value: CmpValue::Mem(512 * 1024 * 1024),
            })]
        );

        let s = structured("restarts>=5");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "restarts".into(),
                op: Op::Ge,
                value: CmpValue::Num(5.0),
            })]
        );
    }

    #[test]
    fn age_durations() {
        let s = structured("age<2h");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "age".into(),
                op: Op::Lt,
                value: CmpValue::Duration(7_200),
            })]
        );

        let s = structured("age>1d2h");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "age".into(),
                op: Op::Gt,
                value: CmpValue::Duration(93_600),
            })]
        );
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("2h"), Some(7_200));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("1w"), Some(604_800));
        assert_eq!(parse_duration("300"), Some(300));
        assert_eq!(parse_duration("2h30"), None); // trailing digits, no unit
        assert_eq!(parse_duration("xyz"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn quantity_parsing() {
        assert_eq!(parse_cpu("250m"), Some(250));
        assert_eq!(parse_cpu("1"), Some(1_000));
        assert_eq!(parse_cpu("1.5"), Some(1_500));
        assert_eq!(parse_cpu("500000000n"), Some(500));
        assert_eq!(parse_cpu("abc"), None);
        assert_eq!(parse_mem("1Ki"), Some(1_024));
        assert_eq!(parse_mem("512Mi"), Some(512 * 1024 * 1024));
        assert_eq!(parse_mem("2000000"), Some(2_000_000));
        assert_eq!(parse_mem("1Xi"), None);
    }

    #[test]
    fn terms_combine_with_and_semantics() {
        let s = structured("api !canary -l app=api status=Running");
        assert_eq!(s.labels.as_deref(), Some("app=api"));
        assert_eq!(s.error, None);
        assert_eq!(
            s.terms,
            vec![
                Term::Fuzzy("api".into()),
                Term::NotFuzzy("canary".into()),
                Term::Cmp(Cmp {
                    key: "status".into(),
                    op: Op::Eq,
                    value: CmpValue::Str("Running".into()),
                }),
            ]
        );
    }

    #[test]
    fn malformed_terms_report_without_blanking() {
        // Mid-typing states must degrade to "term skipped + error", never a
        // hard failure.
        let s = structured("-l");
        assert_eq!(s.labels, None);
        assert!(s.error.as_deref().is_some_and(|e| e.contains("-l")));

        let s = structured("cpu>");
        assert!(s.terms.is_empty());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("cpu>")));

        let s = structured("cpu>abc");
        assert!(s.terms.is_empty());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("abc")));

        let s = structured("age<soon");
        assert!(s.error.as_deref().is_some_and(|e| e.contains("soon")));

        let s = structured("! api");
        assert_eq!(s.terms, vec![Term::Fuzzy("api".into())]);
        assert!(s.error.is_some());
    }

    #[test]
    fn fuzzy_needle_prefers_first_positive_term() {
        assert_eq!(parse("khc").fuzzy_needle(), Some("khc"));
        assert_eq!(parse("").fuzzy_needle(), None);
        assert_eq!(parse("!x khc status=Running").fuzzy_needle(), Some("khc"));
        assert_eq!(parse("-l app=api").fuzzy_needle(), None);
    }

    #[test]
    fn server_side_selectors_only_from_l_and_f() {
        for local in ["api", "status=Running", "!x cpu>1"] {
            let p = parse(local);
            assert_eq!(p.labels(), None, "{local}");
            assert_eq!(p.fields(), None, "{local}");
        }
        assert_eq!(parse("-l app=api").labels(), Some("app=api"));
        assert_eq!(
            parse("-f spec.nodeName=n1").fields(),
            Some("spec.nodeName=n1")
        );
    }
}
