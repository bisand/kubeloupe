//! The time series store: a fixed-window ring of samples per series.
//!
//! There is no TSDB here and there does not need to be one. Lens asks for
//! about twenty distinct metrics over at most 24 hours; on a cluster this
//! size that is a few hundred series, which fits in memory with room to
//! spare. Sizing, for a node running ~40 containers:
//!
//!   ~400 series x (24h / 30s) samples x 16 bytes  ~=  18 MB
//!
//! Series are dropped once every sample has aged out, so pods that come
//! and go do not accumulate.

use std::collections::BTreeMap;
use std::collections::HashMap;

/// Label set of a series, including `__name__`. Ordered so that the
/// canonical key below is stable.
pub type Labels = BTreeMap<String, String>;

pub const NAME_LABEL: &str = "__name__";

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub t: i64,
    pub v: f64,
}

pub struct Series {
    pub labels: Labels,
    /// Ascending by timestamp; the collector only ever appends.
    pub samples: Vec<Sample>,
}

impl Series {
    /// The most recent sample at or before `t`, provided it is not staler
    /// than `lookback`. Prometheus' instant-vector rule, and the reason a
    /// chart flatlines rather than interpolating across a gap.
    pub fn value_at(&self, t: i64, lookback: i64) -> Option<f64> {
        let idx = self.samples.partition_point(|s| s.t <= t);
        let s = self.samples[..idx].last()?;
        (t - s.t <= lookback).then_some(s.v)
    }

    /// Samples in the half-open range `(t - window, t]`.
    pub fn range_at(&self, t: i64, window: i64) -> &[Sample] {
        let end = self.samples.partition_point(|s| s.t <= t);
        let start = self.samples[..end].partition_point(|s| s.t <= t - window);
        &self.samples[start..end]
    }
}

pub struct Store {
    series: HashMap<String, Series>,
    /// `__name__` -> keys, so a named selector never scans the whole store.
    by_name: HashMap<String, Vec<String>>,
    retention: i64,
}

impl Store {
    pub fn new(retention: i64) -> Self {
        Self {
            series: HashMap::new(),
            by_name: HashMap::new(),
            retention,
        }
    }

    pub fn append(&mut self, labels: Labels, t: i64, v: f64) {
        let key = canonical_key(&labels);

        if let Some(series) = self.series.get_mut(&key) {
            // Guard against a duplicate or out-of-order write: the
            // binary searches above assume ascending timestamps.
            if series.samples.last().is_none_or(|s| s.t < t) {
                series.samples.push(Sample { t, v });
            }
            return;
        }

        let name = labels.get(NAME_LABEL).cloned().unwrap_or_default();
        self.by_name.entry(name).or_default().push(key.clone());
        self.series.insert(
            key,
            Series {
                labels,
                samples: vec![Sample { t, v }],
            },
        );
    }

    /// Drop aged-out samples, then any series left empty.
    pub fn prune(&mut self, now: i64) {
        let cutoff = now - self.retention;
        let mut emptied: Vec<String> = Vec::new();

        for (key, series) in self.series.iter_mut() {
            let drop_to = series.samples.partition_point(|s| s.t < cutoff);
            if drop_to > 0 {
                series.samples.drain(..drop_to);
            }
            if series.samples.is_empty() {
                emptied.push(key.clone());
            }
        }

        for key in &emptied {
            if let Some(series) = self.series.remove(key) {
                let name = series.labels.get(NAME_LABEL).cloned().unwrap_or_default();
                if let Some(keys) = self.by_name.get_mut(&name) {
                    keys.retain(|k| k != key);
                    if keys.is_empty() {
                        self.by_name.remove(&name);
                    }
                }
            }
        }
    }

    /// Candidate series for a selector. An exact `__name__` narrows to one
    /// bucket; anything else (`{__name__=~"a|b"}`) has to consider all of
    /// them, which is still only a few hundred.
    pub fn candidates(&self, name: Option<&str>) -> Vec<&Series> {
        match name {
            Some(name) => self
                .by_name
                .get(name)
                .map(|keys| keys.iter().filter_map(|k| self.series.get(k)).collect())
                .unwrap_or_default(),
            None => self.series.values().collect(),
        }
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    pub fn sample_count(&self) -> usize {
        self.series.values().map(|s| s.samples.len()).sum()
    }
}

fn canonical_key(labels: &Labels) -> String {
    let mut key = String::new();
    for (k, v) in labels {
        key.push_str(k);
        key.push('\u{1}');
        key.push_str(v);
        key.push('\u{2}');
    }
    key
}

/// Builds a label set from pairs, skipping empties so that a missing
/// value never produces `label=""` -- Lens filters on `image!=""` and
/// friends, and an empty label would silently drop the series.
pub fn labels(name: &str, pairs: &[(&str, &str)]) -> Labels {
    let mut out = Labels::new();
    out.insert(NAME_LABEL.to_string(), name.to_string());
    for (k, v) in pairs {
        if !v.is_empty() {
            out.insert((*k).to_string(), (*v).to_string());
        }
    }
    out
}
