//! The time series store: a fixed-window history per series, held as a
//! small uncompressed head and a run of compressed chunks behind it.
//!
//! There is no TSDB here and there does not need to be one. Lens asks for
//! about twenty distinct metrics over at most 24 hours; on a cluster this
//! size that is a few hundred series, which fits in memory with room to
//! spare -- and once the older samples are packed down by [`crate::chunk`]
//! it fits several times over. Sizing, for a node running ~40 containers:
//!
//!   ~400 series x (24h / 30s) samples x ~1.3 bytes  ~=  1.5 MB
//!
//! Only the newest [`MAX_CHUNK_SAMPLES`] samples of each series are held
//! raw, so appends stay a pointer bump and instant queries -- which always
//! want the latest point -- never decompress anything.
//!
//! Series are dropped once every sample has aged out, so pods that come
//! and go do not accumulate.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::chunk::{self, Chunk};

/// Label set of a series, including `__name__`. Ordered so that the
/// canonical key below is stable.
pub type Labels = BTreeMap<String, String>;

pub const NAME_LABEL: &str = "__name__";

/// How many samples accumulate before the head is sealed into a chunk. At
/// the default 30s scrape that is two hours, which is long enough for the
/// delta streams to pay for the per-chunk header several times over.
pub const MAX_CHUNK_SAMPLES: usize = 120;

/// Retention is enforced at chunk granularity, so a sealed chunk holds its
/// samples until the *newest* of them ages out. Capping a chunk's span at
/// this fraction of the retention window bounds that overhang -- without
/// it, a short `RETENTION_HOURS` combined with a fast scrape could hold
/// several windows at once.
pub const MAX_CHUNK_SPAN_FRACTION: i64 = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub v: f64,
}

pub struct Series {
    pub labels: Labels,
    /// Sealed and immutable, ascending by time, all older than `head`.
    chunks: Vec<Chunk>,
    /// The newest samples, still raw. Ascending; the collector only appends.
    head: Vec<Sample>,
}

impl Series {
    fn new(labels: Labels) -> Self {
        Self {
            labels,
            chunks: Vec::new(),
            // Deliberately not pre-allocated to `MAX_CHUNK_SAMPLES`: on a
            // churning cluster most series report a handful of times and
            // never fill a head, and doubling tops out one step above the
            // seal point anyway.
            head: Vec::new(),
        }
    }

    /// The most recent sample at or before `t`, provided it is not staler
    /// than `lookback`. Prometheus' instant-vector rule, and the reason a
    /// chart flatlines rather than interpolating across a gap.
    pub fn value_at(&self, t: i64, lookback: i64) -> Option<f64> {
        // The head first: an instant query asks for `now`, which is the
        // one place that never costs a decode.
        let idx = self.head.partition_point(|s| s.t <= t);
        if idx > 0 {
            let sample = self.head[idx - 1];
            return (t - sample.t <= lookback).then_some(sample.v);
        }

        // Otherwise exactly one sealed chunk can hold it: the last one
        // that starts at or before `t`.
        let mut buffer = Vec::new();
        for chunk in self.chunks.iter().rev() {
            if chunk.first_t() > t {
                continue;
            }
            buffer.clear();
            decode(chunk, &mut buffer)?;
            let idx = buffer.partition_point(|s| s.t <= t);
            let sample = buffer[..idx].last()?;
            return (t - sample.t <= lookback).then_some(sample.v);
        }
        None
    }

    /// Samples in the half-open range `(t - window, t]`.
    ///
    /// Returns owned samples rather than a slice: the older ones have to
    /// be decompressed to be read at all. Chunks that fall entirely
    /// outside the window are skipped without decoding, so a 5m `rate()`
    /// over a 24h series still only touches the chunk it lands in.
    pub fn range_at(&self, t: i64, window: i64) -> Vec<Sample> {
        let start = t - window;
        let mut out = Vec::new();

        for chunk in &self.chunks {
            if chunk.last_t() <= start || chunk.first_t() > t {
                continue;
            }
            let mark = out.len();
            if decode(chunk, &mut out).is_none() {
                out.truncate(mark);
            }
        }
        out.retain(|s| s.t > start && s.t <= t);

        let begin = self.head.partition_point(|s| s.t <= start);
        let end = self.head.partition_point(|s| s.t <= t);
        out.extend_from_slice(&self.head[begin..end]);

        out
    }

    pub fn first_t(&self) -> Option<i64> {
        self.chunks
            .first()
            .map(|c| c.first_t())
            .or_else(|| self.head.first().map(|s| s.t))
    }

    pub fn last_t(&self) -> Option<i64> {
        self.head
            .last()
            .map(|s| s.t)
            .or_else(|| self.chunks.last().map(|c| c.last_t()))
    }

    pub fn len(&self) -> usize {
        self.chunks.iter().map(|c| c.len()).sum::<usize>() + self.head.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.head.is_empty()
    }

    /// Every sample, oldest first. Only the tests need a whole series at
    /// once; the snapshot writer copies the chunks across as they are.
    #[cfg(test)]
    pub fn samples(&self) -> Vec<Sample> {
        let mut out = Vec::with_capacity(self.len());
        for chunk in &self.chunks {
            let mark = out.len();
            if decode(chunk, &mut out).is_none() {
                out.truncate(mark);
            }
        }
        out.extend_from_slice(&self.head);
        out
    }

    /// Bytes held on the heap by the samples themselves, for the sizing
    /// tests. Labels are not counted; they are the same either way.
    #[cfg(test)]
    pub fn heap_size(&self) -> usize {
        self.chunks.capacity() * std::mem::size_of::<Chunk>()
            + self.chunks.iter().map(|c| c.heap_size()).sum::<usize>()
            + self.head.capacity() * std::mem::size_of::<Sample>()
    }

    pub fn sealed_chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    pub fn head(&self) -> &[Sample] {
        &self.head
    }

    fn push(&mut self, t: i64, v: f64, retention: i64) {
        self.head.push(Sample { t, v });
        if self.should_seal(retention) {
            self.seal();
        }
    }

    fn should_seal(&self, retention: i64) -> bool {
        if self.head.len() >= MAX_CHUNK_SAMPLES {
            return true;
        }
        match (self.head.first(), self.head.last()) {
            (Some(first), Some(last)) if self.head.len() > 1 => {
                last.t - first.t >= retention / MAX_CHUNK_SPAN_FRACTION
            }
            _ => false,
        }
    }

    fn seal(&mut self) {
        if self.head.is_empty() {
            return;
        }
        let filled = self.head.len();

        // `push` alone would double the capacity. A series holds a
        // predictable number of chunks -- retention divided by a chunk's
        // span -- so the spare half is pure waste at 40 bytes a slot, and
        // growing by exactly one costs one realloc an hour.
        self.chunks.reserve_exact(1);
        self.chunks.push(chunk::encode(&self.head));

        // `clear` keeps the allocation, which is what we want: the next
        // window reuses it instead of growing from nothing. But the head
        // doubled its way up to the seal point and so holds the next power
        // of two -- 128 slots for 120 samples. Trim it to what the window
        // actually reached. Done here rather than up front because a
        // churning pod reports a handful of times and never seals at all.
        self.head.clear();
        if self.head.capacity() > filled {
            self.head.shrink_to_fit();
            self.head.reserve_exact(filled);
        }
    }

    /// Drops everything older than `cutoff`. Sealed chunks go whole -- see
    /// [`MAX_CHUNK_SPAN_FRACTION`] -- while the head is trimmed exactly.
    fn prune(&mut self, cutoff: i64) {
        let aged = self.chunks.partition_point(|c| c.last_t() < cutoff);
        if aged > 0 {
            self.chunks.drain(..aged);
        }
        let drop_to = self.head.partition_point(|s| s.t < cutoff);
        if drop_to > 0 {
            self.head.drain(..drop_to);
        }
    }
}

/// A chunk that fails to decode is a corrupt one, and the snapshot loader
/// rejects those before they reach the store. Skipping rather than
/// unwrapping keeps a bug here from taking the process down mid-query.
fn decode(chunk: &Chunk, out: &mut Vec<Sample>) -> Option<()> {
    match chunk::decode_into(chunk, out) {
        Ok(()) => Some(()),
        Err(error) => {
            debug_assert!(false, "chunk failed to decode: {error}");
            eprintln!("kubeloupe: dropping a chunk that failed to decode: {error:#}");
            None
        }
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
        let retention = self.retention;

        if let Some(series) = self.series.get_mut(&key) {
            // Guard against a duplicate or out-of-order write: the
            // binary searches above assume ascending timestamps.
            if series.last_t().is_none_or(|last| last < t) {
                series.push(t, v, retention);
            }
            return;
        }

        let mut series = Series::new(labels);
        series.push(t, v, retention);
        self.index(key, series);
    }

    /// Drop aged-out samples, then any series left empty.
    pub fn prune(&mut self, now: i64) {
        let cutoff = now - self.retention;
        let mut emptied: Vec<String> = Vec::new();

        for (key, series) in self.series.iter_mut() {
            series.prune(cutoff);
            if series.is_empty() {
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

    /// Bulk-insert a whole series from raw samples, as the v1 snapshot
    /// loader does. Packs everything but the tail straight into chunks.
    pub fn insert_series(&mut self, labels: Labels, samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        let mut series = Series::new(labels);
        for window in samples.chunks(MAX_CHUNK_SAMPLES) {
            series.head.extend_from_slice(window);
            if series.head.len() >= MAX_CHUNK_SAMPLES {
                series.seal();
            }
        }
        self.index(canonical_key(&series.labels), series);
    }

    /// Bulk-insert a series whose chunks are already compressed, as the v2
    /// snapshot loader does. Nothing is re-encoded.
    pub fn insert_chunked(&mut self, labels: Labels, chunks: Vec<Chunk>, head: Vec<Sample>) {
        if chunks.is_empty() && head.is_empty() {
            return;
        }
        let series = Series {
            labels,
            chunks,
            head,
        };
        self.index(canonical_key(&series.labels), series);
    }

    fn index(&mut self, key: String, series: Series) {
        let name = series.labels.get(NAME_LABEL).cloned().unwrap_or_default();
        self.by_name.entry(name).or_default().push(key.clone());
        self.series.insert(key, series);
    }

    pub fn series_iter(&self) -> impl Iterator<Item = &Series> {
        self.series.values()
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    pub fn sample_count(&self) -> usize {
        self.series.values().map(|s| s.len()).sum()
    }

    /// Heap bytes held by samples across every series, which is what the
    /// footprint claim in the README rests on.
    #[cfg(test)]
    pub fn heap_size(&self) -> usize {
        self.series.values().map(|s| s.heap_size()).sum()
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
