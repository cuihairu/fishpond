//! In-process metrics registry (F030 指标): counters and gauges with
//! sorted-label keys, a snapshot for tests/dashboards and a text
//! rendering for logs. No external framework — the export face grows
//! with the control plane (F031 exercises it under load).
//!
//! Label values are data-derived and can absorb caller input, so every
//! label value passes the redaction scanner before it is stored — a
//! secret pasted into a label ends up masked, not persisted.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::redact;

/// What a metric accumulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonic total (requests, redactions).
    Counter,
    /// Point-in-time level (queue depth, occupancy ratio).
    Gauge,
}

impl MetricKind {
    /// Wire name for snapshots and rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// A metric's identity: name plus sorted labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MetricKey {
    /// Metric name (e.g. "fishpond_operations_state").
    pub name: String,
    /// Sorted label pairs.
    pub labels: BTreeMap<String, String>,
}

impl MetricKey {
    /// Builds a key with its labels sorted (the BTreeMap does it).
    pub fn new<'a>(
        name: impl Into<String>,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        Self {
            name: name.into(),
            labels: labels
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    /// Canonical text form, `name{label=value,…}` — labels sorted, so
    /// the same key always renders identically.
    pub fn render(&self) -> String {
        if self.labels.is_empty() {
            return self.name.clone();
        }
        let pairs = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{}{{{pairs}}}", self.name)
    }
}

/// One snapshot row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricPoint {
    /// The metric identity.
    pub key: MetricKey,
    /// Counter or gauge.
    pub kind: MetricKind,
    /// Counter total or gauge level.
    pub value: i64,
}

/// Counters and gauges in one lock, keyed canonically.
#[derive(Default)]
pub struct MetricsRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    counters: BTreeMap<MetricKey, i64>,
    gauges: BTreeMap<MetricKey, i64>,
    /// Secrets to redact from label values, set once at construction.
    known_secrets: Vec<String>,
}

impl MetricsRegistry {
    /// An empty registry without known secrets (shape scan still on).
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that also redacts the given literal secrets from
    /// label values before storing them.
    pub fn with_redaction(known_secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                counters: BTreeMap::new(),
                gauges: BTreeMap::new(),
                known_secrets: known_secrets.into_iter().collect(),
            }),
        }
    }

    /// Adds `by` (default 1) to a counter.
    pub fn inc_counter<'a>(
        &self,
        name: impl Into<String>,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
        by: i64,
    ) {
        self.bump(MetricKey::new(name, labels), MetricKind::Counter, by);
    }

    /// Sets a gauge to `value`.
    pub fn set_gauge<'a>(
        &self,
        name: impl Into<String>,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
        value: i64,
    ) {
        self.bump(MetricKey::new(name, labels), MetricKind::Gauge, value);
    }

    fn bump(&self, key: MetricKey, kind: MetricKind, delta: i64) {
        let mut inner = self.inner.lock().expect("metrics lock");
        let key = self.redact_key(key, &inner.known_secrets);
        let table = match kind {
            MetricKind::Counter => &mut inner.counters,
            MetricKind::Gauge => &mut inner.gauges,
        };
        match kind {
            MetricKind::Counter => *table.entry(key).or_insert(0) += delta,
            MetricKind::Gauge => *table.entry(key).or_insert(0) = delta,
        }
    }

    fn redact_key(&self, mut key: MetricKey, known: &[String]) -> MetricKey {
        for value in key.labels.values_mut() {
            *value = redact::sanitize(value, &known.iter().map(String::as_str).collect::<Vec<_>>());
        }
        key
    }

    /// Every stored point, counters then gauges, canonically ordered.
    pub fn snapshot(&self) -> Vec<MetricPoint> {
        let inner = self.inner.lock().expect("metrics lock");
        let mut points = Vec::new();
        for (key, value) in &inner.counters {
            points.push(MetricPoint {
                key: key.clone(),
                kind: MetricKind::Counter,
                value: *value,
            });
        }
        for (key, value) in &inner.gauges {
            points.push(MetricPoint {
                key: key.clone(),
                kind: MetricKind::Gauge,
                value: *value,
            });
        }
        points
    }

    /// One line per point, `kind name{labels} value` — safe to log.
    pub fn render(&self) -> String {
        self.snapshot()
            .iter()
            .map(|point| {
                format!(
                    "{} {} {}",
                    point.kind.as_str(),
                    point.key.render(),
                    point.value
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_gauges_overwrite() {
        let registry = MetricsRegistry::new();
        registry.inc_counter("requests_total", [("platform", "mock")], 1);
        registry.inc_counter("requests_total", [("platform", "mock")], 2);
        registry.inc_counter("requests_total", [("platform", "other")], 5);
        registry.set_gauge("queue_depth", [("consumer", "projection")], 7);
        registry.set_gauge("queue_depth", [("consumer", "projection")], 3);

        let points = registry.snapshot();
        assert_eq!(points.len(), 3);
        let requests = points
            .iter()
            .filter(|p| p.key.name == "requests_total" && p.key.labels["platform"] == "mock")
            .map(|p| p.value)
            .sum::<i64>();
        assert_eq!(requests, 3);
        let depth = points
            .iter()
            .find(|p| p.key.name == "queue_depth")
            .expect("gauge present");
        assert_eq!((depth.kind, depth.value), (MetricKind::Gauge, 3));
    }

    #[test]
    fn keys_render_canonically_regardless_of_declaration_order() {
        let a = MetricKey::new("m", [("b", "2"), ("a", "1")]);
        let b = MetricKey::new("m", [("a", "1"), ("b", "2")]);
        assert_eq!(a, b);
        assert_eq!(a.render(), "m{a=1,b=2}");
    }

    #[test]
    fn secret_labels_are_masked_before_storage() {
        let secret = "fp_live_label_secret";
        let registry = MetricsRegistry::with_redaction([secret.to_owned()]);
        let note = format!("tenant {secret}");
        registry.set_gauge("tenant_state", [("note", note.as_str())], 1);

        let rendered = registry.render();
        assert!(!rendered.contains(secret), "{rendered}");
        // The shape scan is on even without known secrets.
        let registry2 = MetricsRegistry::new();
        registry2.set_gauge("url_gauge", [("endpoint", "postgres://u:pw@host/x")], 1);
        assert!(
            !registry2.render().contains(":pw@"),
            "{}",
            registry2.render()
        );
    }

    #[test]
    fn render_is_log_safe_and_stable() {
        let registry = MetricsRegistry::new();
        registry.inc_counter("c", [], 1);
        registry.set_gauge("g", [("k", "v")], 2);
        let first = registry.render();
        let again = registry.render();
        assert_eq!(first, again);
        assert!(first.contains("counter c 1"));
        assert!(first.contains("gauge g{k=v} 2"));
    }
}
