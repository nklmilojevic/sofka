//! Screen dumps and structured snapshots.
//!
//! A snapshot is a point-in-time capture of the current table view — its
//! metadata (context, namespace, resource, filter) plus the visible columns
//! and rows — saved to disk for later inspection and browsable from
//! `:snapshots`. Unlike the one-frame `--snapshot` CI mode, this is an
//! interactive capture-and-review workflow. Captures can be written as plain
//! text (an aligned table), JSON, or YAML.

use std::path::PathBuf;

use serde::Serialize;

/// Output format for a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Aligned plain-text table (a text screen dump).
    Text,
    Json,
    Yaml,
}

impl Format {
    /// Parse a format name (`text`/`txt`, `json`, `yaml`/`yml`); `None` for
    /// anything else.
    pub fn parse(s: &str) -> Option<Format> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "text" | "txt" => Some(Format::Text),
            "json" => Some(Format::Json),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Text => "txt",
            Format::Json => "json",
            Format::Yaml => "yaml",
        }
    }
}

/// A captured table view.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Epoch seconds at capture.
    pub captured_at: i64,
    pub context: String,
    pub cluster: String,
    /// Namespace scope, empty when listing across all namespaces.
    pub namespace: String,
    /// Resource plural.
    pub resource: String,
    /// Active row filter, if any.
    pub filter: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Snapshot {
    /// Serialize into the requested format.
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Text => self.to_text(),
            Format::Json => {
                serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("// error: {e}"))
            }
            Format::Yaml => serde_yaml::to_string(self).unwrap_or_else(|e| format!("# error: {e}")),
        }
    }

    /// A human-readable header block plus the rows as an aligned table.
    fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# sofka snapshot — {}\n", self.resource));
        out.push_str(&format!("# captured: {}\n", clock(self.captured_at)));
        out.push_str(&format!("# context:  {}\n", self.context));
        out.push_str(&format!("# cluster:  {}\n", self.cluster));
        out.push_str(&format!(
            "# namespace: {}\n",
            if self.namespace.is_empty() {
                "(all)"
            } else {
                &self.namespace
            }
        ));
        if !self.filter.is_empty() {
            out.push_str(&format!("# filter:   {}\n", self.filter));
        }
        out.push_str(&format!("# rows:     {}\n\n", self.rows.len()));
        out.push_str(&align_table(&self.columns, &self.rows));
        out
    }

    /// A default filename for this snapshot in the given format.
    pub fn filename(&self, format: Format) -> String {
        let ns = if self.namespace.is_empty() {
            "all".to_string()
        } else {
            self.namespace.clone()
        };
        let safe: String = format!("{}-{}", self.resource, ns)
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        format!(
            "sofka-snapshot-{safe}-{}.{}",
            self.captured_at,
            format.ext()
        )
    }
}

/// Render `columns` + `rows` as a left-aligned, space-padded table.
fn align_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let ncols = columns.len();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, &w) in widths.iter().enumerate() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            line.push_str(cell);
            // No trailing padding on the last column.
            if i + 1 < ncols {
                let pad = w.saturating_sub(cell.chars().count());
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        line.trim_end().to_string()
    };
    let mut out = String::new();
    out.push_str(&fmt_row(columns));
    out.push('\n');
    for row in rows {
        out.push_str(&fmt_row(row));
        out.push('\n');
    }
    out
}

/// Format an epoch second as `YYYY-MM-DD HH:MM:SS` UTC.
pub fn clock(at: i64) -> String {
    use k8s_openapi::jiff::Timestamp;
    match Timestamp::from_second(at) {
        Ok(ts) => ts
            .to_string()
            .split('.')
            .next()
            .unwrap_or("")
            .replace('T', " ")
            .replace('Z', ""),
        Err(_) => at.to_string(),
    }
}

/// A compact "5m ago" / "2h ago" age for `at` relative to `now` (epoch secs).
pub fn age(at: i64, now: i64) -> String {
    let secs = (now - at).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Directory snapshots are written to and browsed from:
/// `<state-dir>/snapshots` (see [`crate::diagnostics::state_dir`]).
pub fn snapshots_dir() -> PathBuf {
    crate::diagnostics::state_dir().join("snapshots")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            captured_at: 1_700_000_000,
            context: "prod".into(),
            cluster: "eu".into(),
            namespace: "web".into(),
            resource: "pods".into(),
            filter: "app=api".into(),
            columns: vec!["NAME".into(), "READY".into()],
            rows: vec![
                vec!["api-1".into(), "1/1".into()],
                vec!["api-longer-name".into(), "0/1".into()],
            ],
        }
    }

    #[test]
    fn text_render_aligns_and_includes_metadata() {
        let text = sample().render(Format::Text);
        assert!(text.contains("# context:  prod"));
        assert!(text.contains("# filter:   app=api"));
        assert!(text.contains("# rows:     2"));
        // The NAME column is padded to the widest cell.
        let body = text.lines().find(|l| l.starts_with("api-1")).unwrap();
        assert!(body.starts_with("api-1           "), "{body:?}");
        assert!(body.contains("1/1"));
    }

    #[test]
    fn json_and_yaml_roundtrip_the_fields() {
        let json = sample().render(Format::Json);
        assert!(json.contains("\"resource\": \"pods\""));
        assert!(json.contains("\"api-longer-name\""));
        let yaml = sample().render(Format::Yaml);
        assert!(yaml.contains("resource: pods"));
        assert!(yaml.contains("- api-1"));
    }

    #[test]
    fn format_parse_accepts_aliases_and_rejects_garbage() {
        assert_eq!(Format::parse(""), Some(Format::Text));
        assert_eq!(Format::parse("TXT"), Some(Format::Text));
        assert_eq!(Format::parse("json"), Some(Format::Json));
        assert_eq!(Format::parse("yml"), Some(Format::Yaml));
        assert_eq!(Format::parse("xml"), None);
    }

    #[test]
    fn filename_is_filesystem_safe_and_carries_the_extension() {
        let s = sample();
        assert_eq!(
            s.filename(Format::Json),
            "sofka-snapshot-pods-web-1700000000.json"
        );
    }

    #[test]
    fn age_buckets_by_unit() {
        assert_eq!(age(100, 130), "30s ago");
        assert_eq!(age(0, 120), "2m ago");
        assert_eq!(age(0, 7200), "2h ago");
        assert_eq!(age(0, 172_800), "2d ago");
        assert_eq!(age(200, 100), "0s ago"); // clamps negatives
    }
}
