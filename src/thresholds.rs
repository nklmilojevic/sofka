//! User-configurable warning/critical thresholds for value-based cell
//! coloring. These decide when a RESTARTS/CPU/MEM cell (and the container
//! picker's request/limit utilization) stops reading as healthy and turns a
//! warning or critical tint.
//!
//! Thresholds come from config (see [`crate::config::Thresholds`]) with three
//! layers of precedence, low to high:
//!
//! 1. Built-in defaults ([`Thresholds::default`]) — the historical hardcoded
//!    values, so an empty config behaves exactly as before.
//! 2. Global `[thresholds]` overrides.
//! 3. Per-resource `[thresholds.resources.<key>]` overrides, keyed like
//!    `[views]` (`apiVersion/plural`, `group/plural`, plural, or lowercased
//!    kind — most specific wins).
//!
//! Per-context overrides come for free: config override files merge before we
//! compile, exactly like the rest of the configuration.
//!
//! ```toml
//! [thresholds]
//! restarts = { warn = 3, critical = 10 }
//! cpu = { warn = "200m", critical = "1" }
//! memory = { warn = "256Mi", critical = "1Gi" }
//! utilization = { warn = 75, critical = 90 }
//!
//! [thresholds.resources.pods]
//! restarts = { warn = 5, critical = 20 }
//! ```

use std::collections::HashMap;

use kube::discovery::ApiResource;

use crate::columns::{parse_cpu_milli, parse_mem_bytes};
use crate::config;

/// Which side of the thresholds a measured value falls on. `None` (from
/// [`Band::severity`]) means "below the warning line" — healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Critical,
}

/// A warn/critical band over an ordered `i64` metric (restart counts,
/// millicores, bytes, percentages). Both bounds are inclusive lower limits;
/// either can be `None` to disable that level entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Band {
    pub warn: Option<i64>,
    pub critical: Option<i64>,
}

impl Band {
    const fn new(warn: i64, critical: i64) -> Self {
        Band {
            warn: Some(warn),
            critical: Some(critical),
        }
    }

    /// Severity of `value`: `Critical` at or above the critical bound, else
    /// `Warn` at or above the warn bound, else `None` (healthy). Critical is
    /// checked first so an out-of-order config (`critical < warn`) still
    /// escalates rather than capping at warn.
    pub fn severity(&self, value: i64) -> Option<Severity> {
        if let Some(c) = self.critical
            && value >= c
        {
            return Some(Severity::Critical);
        }
        if let Some(w) = self.warn
            && value >= w
        {
            return Some(Severity::Warn);
        }
        None
    }

    /// Overlay individually-set bounds from a config band, keeping the base
    /// value where the override omits one.
    fn overlaid_num(self, o: &config::CountBand) -> Self {
        Band {
            warn: o.warn.or(self.warn),
            critical: o.critical.or(self.critical),
        }
    }

    /// As [`Self::overlaid_num`] but for quantity strings (CPU/memory),
    /// parsing each present bound with `parse`. Unparseable values are left as
    /// the base and reported through `warn_sink`.
    fn overlaid_qty(
        self,
        o: &config::QuantityBand,
        metric: &str,
        parse: impl Fn(&str) -> i64,
        warn_sink: &mut Vec<String>,
    ) -> Self {
        let parse_bound = |field: &str, raw: &Option<String>, sink: &mut Vec<String>| match raw {
            None => None,
            Some(s) => {
                let v = parse(s);
                if v > 0 {
                    Some(v)
                } else {
                    sink.push(format!(
                        "thresholds.{metric}.{field}: invalid quantity '{s}' (ignored)"
                    ));
                    None
                }
            }
        };
        Band {
            warn: parse_bound("warn", &o.warn, warn_sink).or(self.warn),
            critical: parse_bound("critical", &o.critical, warn_sink).or(self.critical),
        }
    }
}

/// A fully-resolved set of thresholds for one resource view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Summed container restart count (RESTARTS cell).
    pub restarts: Band,
    /// Absolute CPU usage in millicores (CPU cell).
    pub cpu: Band,
    /// Absolute memory usage in bytes (MEM cell).
    pub memory: Band,
    /// Usage as a percentage of a container's request/limit (container picker).
    pub utilization: Band,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            restarts: Band::new(1, 5),
            cpu: Band::new(200, 1000),
            memory: Band::new(256 * 1024 * 1024, 1024 * 1024 * 1024),
            utilization: Band::new(75, 90),
        }
    }
}

impl Thresholds {
    /// Overlay a config threshold set onto these bands, parsing CPU/memory
    /// quantities and collecting any parse warnings.
    fn overlaid(self, set: &config::ThresholdSet, warnings: &mut Vec<String>) -> Self {
        Thresholds {
            restarts: set
                .restarts
                .map_or(self.restarts, |b| self.restarts.overlaid_num(&b)),
            cpu: set.cpu.as_ref().map_or(self.cpu, |b| {
                self.cpu.overlaid_qty(b, "cpu", parse_cpu_milli, warnings)
            }),
            memory: set.memory.as_ref().map_or(self.memory, |b| {
                self.memory
                    .overlaid_qty(b, "memory", parse_mem_bytes, warnings)
            }),
            utilization: set
                .utilization
                .map_or(self.utilization, |b| self.utilization.overlaid_num(&b)),
        }
    }
}

/// Compiled thresholds: resolved global defaults plus per-resource overrides,
/// resolved against a resource kind on demand (mirrors [`crate::views`]).
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    default: Thresholds,
    resources: HashMap<String, Thresholds>,
}

impl Compiled {
    /// The thresholds that apply to `ar`, most specific key first:
    /// `apiVersion/plural`, `group/plural`, plural, then lowercased kind.
    /// Falls back to the resolved global defaults.
    pub fn resolve(&self, ar: &ApiResource) -> Thresholds {
        if self.resources.is_empty() {
            return self.default;
        }
        let plural = ar.plural.to_lowercase();
        let mut keys = vec![format!("{}/{plural}", ar.api_version.to_lowercase())];
        if !ar.group.is_empty() {
            keys.push(format!("{}/{plural}", ar.group.to_lowercase()));
        }
        keys.push(plural);
        keys.push(ar.kind.to_lowercase());
        keys.iter()
            .find_map(|k| self.resources.get(k))
            .copied()
            .unwrap_or(self.default)
    }

    /// The resolved global defaults, for callers without a resource kind.
    pub fn defaults(&self) -> Thresholds {
        self.default
    }
}

/// Compile the raw `[thresholds]` config into resolved bands. Invalid quantity
/// values are skipped with an actionable warning instead of dropping the whole
/// config.
pub fn compile(cfg: &config::Thresholds) -> (Compiled, Vec<String>) {
    let mut warnings = Vec::new();
    let default = Thresholds::default().overlaid(&cfg.defaults, &mut warnings);
    let resources =
        cfg.resources
            .iter()
            .map(|(key, set)| {
                let mut per = Vec::new();
                let resolved = default.overlaid(set, &mut per);
                // Attribute per-resource parse errors to their subtable.
                warnings.extend(per.into_iter().map(|w| {
                    w.replacen("thresholds.", &format!("thresholds.resources.{key}."), 1)
                }));
                (key.to_lowercase(), resolved)
            })
            .collect();
    (Compiled { default, resources }, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CountBand, QuantityBand, ThresholdSet, Thresholds as Cfg};

    fn ar(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: version.into(),
            api_version: if group.is_empty() {
                version.into()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    #[test]
    fn band_severity_bounds_are_inclusive_and_critical_wins() {
        let b = Band::new(1, 5);
        assert_eq!(b.severity(0), None);
        assert_eq!(b.severity(1), Some(Severity::Warn));
        assert_eq!(b.severity(4), Some(Severity::Warn));
        assert_eq!(b.severity(5), Some(Severity::Critical));
        assert_eq!(b.severity(99), Some(Severity::Critical));
    }

    #[test]
    fn disabled_bound_is_skipped() {
        let warn_only = Band {
            warn: Some(3),
            critical: None,
        };
        assert_eq!(warn_only.severity(100), Some(Severity::Warn));
        let none = Band::default();
        assert_eq!(none.severity(i64::MAX), None);
    }

    #[test]
    fn empty_config_keeps_builtin_defaults() {
        let (c, warns) = compile(&Cfg::default());
        assert!(warns.is_empty());
        assert_eq!(c.defaults(), Thresholds::default());
        // Unknown kind still resolves to defaults.
        assert_eq!(
            c.resolve(&ar("", "v1", "Pod", "pods")),
            Thresholds::default()
        );
    }

    #[test]
    fn global_overrides_merge_per_bound_and_parse_quantities() {
        let cfg = Cfg {
            defaults: ThresholdSet {
                restarts: Some(CountBand {
                    warn: Some(3),
                    critical: None, // keep built-in 5
                }),
                cpu: Some(QuantityBand {
                    warn: Some("500m".into()),
                    critical: Some("2".into()),
                }),
                memory: Some(QuantityBand {
                    warn: Some("1Gi".into()),
                    critical: None,
                }),
                utilization: None,
            },
            resources: Default::default(),
        };
        let (c, warns) = compile(&cfg);
        assert!(warns.is_empty(), "{warns:?}");
        let t = c.defaults();
        assert_eq!(t.restarts, Band::new(3, 5));
        assert_eq!(t.cpu, Band::new(500, 2000));
        assert_eq!(t.memory.warn, Some(1024 * 1024 * 1024));
        assert_eq!(t.memory.critical, Some(1024 * 1024 * 1024)); // built-in kept
        assert_eq!(t.utilization, Thresholds::default().utilization);
    }

    #[test]
    fn per_resource_overlays_over_globals() {
        let mut resources = HashMap::new();
        resources.insert(
            "pods".to_string(),
            ThresholdSet {
                restarts: Some(CountBand {
                    warn: Some(10),
                    critical: Some(50),
                }),
                ..Default::default()
            },
        );
        let cfg = Cfg {
            defaults: ThresholdSet {
                cpu: Some(QuantityBand {
                    warn: Some("100m".into()),
                    critical: Some("500m".into()),
                }),
                ..Default::default()
            },
            resources,
        };
        let (c, warns) = compile(&cfg);
        assert!(warns.is_empty());
        let pods = c.resolve(&ar("", "v1", "Pod", "pods"));
        // Per-resource restart band applied...
        assert_eq!(pods.restarts, Band::new(10, 50));
        // ...global CPU override inherited by the per-resource set.
        assert_eq!(pods.cpu, Band::new(100, 500));
        // A different kind gets only the globals.
        let deploy = c.resolve(&ar("apps", "v1", "Deployment", "deployments"));
        assert_eq!(deploy.restarts, Thresholds::default().restarts);
        assert_eq!(deploy.cpu, Band::new(100, 500));
    }

    #[test]
    fn invalid_quantity_warns_and_keeps_default() {
        let cfg = Cfg {
            defaults: ThresholdSet {
                cpu: Some(QuantityBand {
                    warn: Some("banana".into()),
                    critical: Some("1".into()),
                }),
                ..Default::default()
            },
            resources: Default::default(),
        };
        let (c, warns) = compile(&cfg);
        assert_eq!(warns.len(), 1);
        assert!(
            warns[0].contains("cpu.warn") && warns[0].contains("banana"),
            "{warns:?}"
        );
        // The bad warn bound falls back to the built-in; the good one applies.
        assert_eq!(c.defaults().cpu, Band::new(200, 1000));
    }

    #[test]
    fn per_resource_parse_warning_is_attributed_to_subtable() {
        let mut resources = HashMap::new();
        resources.insert(
            "pods".to_string(),
            ThresholdSet {
                memory: Some(QuantityBand {
                    warn: Some("nope".into()),
                    critical: None,
                }),
                ..Default::default()
            },
        );
        let cfg = Cfg {
            resources,
            ..Default::default()
        };
        let (_c, warns) = compile(&cfg);
        assert_eq!(warns.len(), 1);
        assert!(
            warns[0].contains("thresholds.resources.pods.memory.warn"),
            "{warns:?}"
        );
    }
}
