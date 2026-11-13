//! Evaluation of the parsed subset.
//!
//! A query is evaluated in two passes. First every selector in it reads
//! its samples once, for the whole time range the query will look at --
//! from memory and, when there is one, from the disk tier. Then each step
//! of a range query evaluates against those, by binary search. Reading
//! per step instead would decode the same chunks once per point, and on
//! disk would read the same bytes a thousand times for a 30-day chart.

use super::{AggOp, BinOp, Expr, RangeFn, Selector};
use crate::store::{Labels, NAME_LABEL, Sample, Store};
use std::collections::{BTreeMap, HashMap, HashSet};

/// How stale a sample may be and still answer for an instant. Prometheus'
/// default, and what makes a chart break rather than draw a flat line
/// across a collector outage.
const LOOKBACK: i64 = 300;

pub enum Value {
    Scalar(f64),
    Vector(Vec<(Labels, f64)>),
}

impl Value {
    fn into_vector(self) -> Vec<(Labels, f64)> {
        match self {
            Value::Vector(v) => v,
            Value::Scalar(_) => Vec::new(),
        }
    }
}

/// Every selector's series, by selector id, with the name already dropped:
/// PromQL drops it from anything a function or operator touches, and
/// matching two vectors line by line depends on it being gone.
pub struct Fetched {
    series: Vec<Vec<Found>>,
    /// How far back an instant looks.
    lookback: i64,
    /// The step of a range query, 0 for an instant one. A window narrower
    /// than this is widened to it, as VictoriaMetrics does, so that a
    /// `rate(x[1m])` over a month averages each step rather than sampling
    /// one minute of it.
    step: i64,
}

struct Found {
    labels: Labels,
    samples: Vec<Sample>,
    /// For range functions: the counter-aware increase from the first
    /// sample to each one, so any window's increase is one subtraction --
    /// `increase(x[30d])` charted at a thousand steps would otherwise sum
    /// a month of samples a thousand times.
    climb: Vec<f64>,
}

impl Found {
    /// The counter-aware increase across `samples[from..end]`.
    fn delta(&self, from: usize, end: usize) -> f64 {
        self.climb[end - 1] - self.climb[from]
    }
}

/// Reads what `expr` needs to be evaluated anywhere in `[start, end]`.
pub fn fetch(store: &Store, expr: &Expr, start: i64, end: i64, step: i64) -> Fetched {
    let mut wanted = Vec::new();
    selectors(expr, &mut wanted);
    let widest = wanted.iter().filter_map(|(_, w)| *w).max().unwrap_or(0);

    // Downsampled history has a point every few minutes: an instant that
    // looks back less than two of those, or a window narrower than two,
    // would find nothing there at all.
    let resolution = store.resolution_since(start - widest.max(step) - LOOKBACK);
    let mut fetched = Fetched {
        series: Vec::new(),
        lookback: LOOKBACK.max(step).max(2 * resolution),
        step: step.max(2 * resolution),
    };

    for (selector, window) in wanted {
        let reach = window.map_or(0, |w| fetched.window(w)) + fetched.lookback;
        let found: Vec<Found> = store
            .fetch(selector, start - reach, end)
            .into_iter()
            .map(|(mut labels, samples)| {
                labels.remove(NAME_LABEL);
                let climb = if window.is_some() {
                    climb(&samples)
                } else {
                    Vec::new()
                };
                Found {
                    labels,
                    samples,
                    climb,
                }
            })
            .collect();
        if fetched.series.len() <= selector.id {
            fetched.series.resize_with(selector.id + 1, Vec::new);
        }
        fetched.series[selector.id] = found;
    }
    fetched
}

fn selectors<'a>(expr: &'a Expr, out: &mut Vec<(&'a Selector, Option<i64>)>) {
    match expr {
        Expr::Selector(selector) => out.push((selector, None)),
        Expr::Range {
            selector, window, ..
        } => out.push((selector, Some(*window))),
        Expr::Agg { expr, .. } | Expr::Vector(expr) => selectors(expr, out),
        Expr::Clamp { expr, bound, .. } => {
            selectors(expr, out);
            selectors(bound, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            selectors(lhs, out);
            selectors(rhs, out);
        }
        Expr::Time | Expr::Number(_) => {}
    }
}

impl Fetched {
    fn window(&self, window: i64) -> i64 {
        window.max(self.step)
    }

    fn of(&self, selector: &Selector) -> &[Found] {
        self.series
            .get(selector.id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Instant query: evaluates `expr` at `t`.
pub fn eval(store: &Store, expr: &Expr, t: i64) -> Value {
    let fetched = fetch(store, expr, t, t, 0);
    eval_at(&fetched, expr, t)
}

pub fn eval_at(data: &Fetched, expr: &Expr, t: i64) -> Value {
    match expr {
        Expr::Number(n) => Value::Scalar(*n),
        Expr::Time => Value::Scalar(t as f64),

        Expr::Selector(selector) => {
            let mut out = Vec::new();
            for found in data.of(selector) {
                let samples = &found.samples;
                let idx = samples.partition_point(|s| s.t <= t);
                if idx > 0 && t - samples[idx - 1].t <= data.lookback {
                    out.push((found.labels.clone(), samples[idx - 1].v));
                }
            }
            Value::Vector(out)
        }

        Expr::Range {
            func,
            selector,
            window,
        } => {
            let window = data.window(*window);
            let mut out = Vec::new();
            for found in data.of(selector) {
                let samples = &found.samples;
                let begin = samples.partition_point(|s| s.t <= t - window);
                let end = samples.partition_point(|s| s.t <= t);
                let rate = |from: usize| {
                    let span = samples[end - 1].t - samples[from].t;
                    (end >= from + 2 && span > 0).then(|| found.delta(from, end) / span as f64)
                };
                let value = match func {
                    _ if end == 0 => None,
                    RangeFn::Rate => rate(begin),
                    RangeFn::Irate => rate(begin.max(end.saturating_sub(2))),
                    // VictoriaMetrics' increase: from the last sample before
                    // the window, when there is a recent one, so consecutive
                    // windows add up to the whole and nothing falls between.
                    RangeFn::Increase => {
                        let from =
                            if begin > 0 && (t - window) - samples[begin - 1].t <= data.lookback {
                                begin - 1
                            } else {
                                begin
                            };
                        (end >= from + 2).then(|| found.delta(from, end))
                    }
                };
                if let Some(value) = value {
                    out.push((found.labels.clone(), value));
                }
            }
            Value::Vector(out)
        }

        Expr::Agg {
            op,
            expr,
            by,
            without,
        } => {
            let input = eval_at(data, expr, t).into_vector();
            let mut groups: HashMap<String, (Labels, f64, usize)> = HashMap::new();

            for (labels, value) in input {
                // No clause folds everything into one series. So does
                // `by (kubernetes_name)`, a label nothing here carries --
                // which is exactly what Lens' cluster memory query wants.
                let grouped: Labels = match (by, without) {
                    (Some(names), false) => names
                        .iter()
                        .filter_map(|n| labels.get(n).map(|v| (n.clone(), v.clone())))
                        .collect(),
                    (Some(names), true) => labels
                        .into_iter()
                        .filter(|(k, _)| !names.contains(k))
                        .collect(),
                    (None, _) => BTreeMap::new(),
                };

                let key = key_of(&grouped);
                groups
                    .entry(key)
                    .and_modify(|(_, acc, n)| {
                        *n += 1;
                        *acc = match op {
                            AggOp::Sum | AggOp::Avg => *acc + value,
                            AggOp::Min => acc.min(value),
                            AggOp::Max => acc.max(value),
                            AggOp::Count => *n as f64,
                        };
                    })
                    .or_insert_with(|| {
                        let first = if *op == AggOp::Count { 1.0 } else { value };
                        (grouped, first, 1)
                    });
            }

            Value::Vector(
                groups
                    .into_values()
                    .map(|(labels, acc, n)| {
                        let value = if *op == AggOp::Avg {
                            acc / n as f64
                        } else {
                            acc
                        };
                        (labels, value)
                    })
                    .collect(),
            )
        }

        Expr::Clamp { expr, bound, min } => {
            let Value::Scalar(bound) = eval_at(data, bound, t) else {
                return Value::Vector(Vec::new());
            };
            let clamp = |v: f64| if *min { v.max(bound) } else { v.min(bound) };
            match eval_at(data, expr, t) {
                Value::Scalar(v) => Value::Scalar(clamp(v)),
                Value::Vector(v) => {
                    Value::Vector(v.into_iter().map(|(l, x)| (l, clamp(x))).collect())
                }
            }
        }

        Expr::Vector(inner) => match eval_at(data, inner, t) {
            Value::Scalar(v) => Value::Vector(vec![(Labels::new(), v)]),
            vector => vector,
        },

        Expr::Binary { op, lhs, rhs } => {
            let lhs = eval_at(data, lhs, t);
            let rhs = eval_at(data, rhs, t);
            binary(*op, lhs, rhs)
        }
    }
}

fn binary(op: BinOp, lhs: Value, rhs: Value) -> Value {
    if matches!(op, BinOp::And | BinOp::Or | BinOp::Unless) {
        let (a, b) = (lhs.into_vector(), rhs.into_vector());
        let keys =
            |v: &[(Labels, f64)]| -> HashSet<String> { v.iter().map(|(l, _)| key_of(l)).collect() };
        return Value::Vector(match op {
            BinOp::And => {
                let in_b = keys(&b);
                a.into_iter()
                    .filter(|(l, _)| in_b.contains(&key_of(l)))
                    .collect()
            }
            BinOp::Unless => {
                let in_b = keys(&b);
                a.into_iter()
                    .filter(|(l, _)| !in_b.contains(&key_of(l)))
                    .collect()
            }
            _ => {
                let in_a = keys(&a);
                let mut out = a;
                out.extend(b.into_iter().filter(|(l, _)| !in_a.contains(&key_of(l))));
                out
            }
        });
    }

    match (lhs, rhs) {
        (Value::Scalar(a), Value::Scalar(b)) => Value::Scalar(match compare(op, a, b) {
            Some(holds) => f64::from(u8::from(holds)),
            None => apply(op, a, b),
        }),
        (Value::Vector(v), Value::Scalar(s)) => Value::Vector(
            v.into_iter()
                .filter_map(|(l, x)| combine(op, x, s, x).map(|r| (l, r)))
                .collect(),
        ),
        (Value::Scalar(s), Value::Vector(v)) => Value::Vector(
            v.into_iter()
                .filter_map(|(l, x)| combine(op, s, x, x).map(|r| (l, r)))
                .collect(),
        ),
        (Value::Vector(a), Value::Vector(b)) => {
            // One-to-one matching on the full label set, which is what
            // makes `MemTotal - (MemFree + Buffers + Cached)` line up:
            // every one of those carries the same kubernetes_node and
            // instance.
            let rhs_by_labels: HashMap<String, f64> =
                b.into_iter().map(|(l, v)| (key_of(&l), v)).collect();

            let mut out = Vec::new();
            for (labels, value) in a {
                if let Some(other) = rhs_by_labels.get(&key_of(&labels))
                    && let Some(result) = combine(op, value, *other, value)
                {
                    out.push((labels, result));
                }
            }
            Value::Vector(out)
        }
    }
}

/// Arithmetic gives its result; a comparison filters, keeping `keep` (the
/// vector side's own value) where it holds.
fn combine(op: BinOp, a: f64, b: f64, keep: f64) -> Option<f64> {
    match compare(op, a, b) {
        Some(true) => Some(keep),
        Some(false) => None,
        None => Some(apply(op, a, b)),
    }
}

fn compare(op: BinOp, a: f64, b: f64) -> Option<bool> {
    Some(match op {
        BinOp::Eq => a == b,
        BinOp::Ne => a != b,
        BinOp::Gt => a > b,
        BinOp::Lt => a < b,
        BinOp::Ge => a >= b,
        BinOp::Le => a <= b,
        _ => return None,
    })
}

/// Running counter-aware increase. A reset -- a container restart zeroes
/// its CPU counter -- counts the new value from zero, where a plain
/// last-minus-first would render it as a large negative rate.
fn climb(samples: &[Sample]) -> Vec<f64> {
    let mut out = Vec::with_capacity(samples.len());
    let mut total = 0.0;
    let mut prev = samples.first().map_or(0.0, |s| s.v);
    for sample in samples {
        total += if sample.v < prev {
            sample.v
        } else {
            sample.v - prev
        };
        prev = sample.v;
        out.push(total);
    }
    out
}

/// The series a selector matches in memory, for the discovery endpoints.
/// Queries go through [`fetch`], which also reads disk.
pub(crate) fn select<'a>(store: &'a Store, selector: &Selector) -> Vec<&'a crate::store::Series> {
    store
        .candidates(selector.name.as_deref())
        .into_iter()
        .filter(|series| matches(selector, &series.labels))
        .collect()
}

pub(crate) fn matches(selector: &Selector, labels: &Labels) -> bool {
    if let Some(name) = &selector.name
        && labels.get(NAME_LABEL) != Some(name)
    {
        return false;
    }
    selector.matchers.iter().all(|matcher| {
        // Prometheus treats an absent label as the empty string, which is
        // what makes `container!=""` exclude the pod-level rollups rather
        // than everything.
        let value = labels.get(&matcher.label).map(String::as_str).unwrap_or("");
        matcher.matches(value)
    })
}

fn key_of(labels: &Labels) -> String {
    let mut key = String::new();
    for (k, v) in labels {
        key.push_str(k);
        key.push('\u{1}');
        key.push_str(v);
        key.push('\u{2}');
    }
    key
}

fn apply(op: BinOp, a: f64, b: f64) -> f64 {
    match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        _ => f64::NAN,
    }
}

/// A matrix result: one label set, and the points that answered for it.
pub struct RangeSeries {
    pub labels: Labels,
    pub values: Vec<(i64, f64)>,
}

pub fn query_range(
    store: &Store,
    expr: &Expr,
    start: i64,
    end: i64,
    step: i64,
) -> Vec<RangeSeries> {
    let step = step.max(1);
    let fetched = fetch(store, expr, start, end, step);
    let mut ordered: Vec<String> = Vec::new();
    let mut series: HashMap<String, RangeSeries> = HashMap::new();

    let mut t = start;
    while t <= end {
        for (labels, value) in eval_at(&fetched, expr, t).into_vector() {
            if !value.is_finite() {
                continue;
            }
            let key = key_of(&labels);
            series
                .entry(key.clone())
                .or_insert_with(|| {
                    ordered.push(key);
                    RangeSeries {
                        labels,
                        values: Vec::new(),
                    }
                })
                .values
                .push((t, value));
        }
        t += step;
    }

    ordered
        .into_iter()
        .filter_map(|key| series.remove(&key))
        .collect()
}
