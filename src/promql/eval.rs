//! Evaluation of the parsed subset against the store.

use super::{BinOp, Expr, Selector};
use crate::store::{Labels, NAME_LABEL, Store};
use std::collections::BTreeMap;
use std::collections::HashMap;

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

pub fn eval(store: &Store, expr: &Expr, t: i64) -> Value {
    match expr {
        Expr::Number(n) => Value::Scalar(*n),

        Expr::Selector(selector) => {
            let mut out = Vec::new();
            for series in select(store, selector) {
                if let Some(v) = series.value_at(t, LOOKBACK) {
                    out.push((without_name(&series.labels), v));
                }
            }
            Value::Vector(out)
        }

        Expr::Rate(selector, window) => {
            let mut out = Vec::new();
            for series in select(store, selector) {
                let samples = series.range_at(t, *window);
                if samples.len() < 2 {
                    continue;
                }
                let span = samples[samples.len() - 1].t - samples[0].t;
                if span <= 0 {
                    continue;
                }

                // Counter-aware delta: a container restart resets its
                // CPU counter, and a plain last-minus-first would render
                // that as a large negative rate.
                let mut delta = 0.0;
                let mut prev = samples[0].v;
                for sample in &samples[1..] {
                    delta += if sample.v < prev {
                        sample.v
                    } else {
                        sample.v - prev
                    };
                    prev = sample.v;
                }

                out.push((without_name(&series.labels), delta / span as f64));
            }
            Value::Vector(out)
        }

        Expr::Sum { expr, by } => {
            let input = eval(store, expr, t).into_vector();
            let mut groups: HashMap<String, (Labels, f64)> = HashMap::new();

            for (labels, value) in input {
                // No `by` clause folds everything into one series. So does
                // `by (kubernetes_name)`, a label nothing here carries --
                // which is exactly what Lens' cluster memory query wants.
                let grouped = match by {
                    Some(names) => {
                        let mut kept = BTreeMap::new();
                        for name in names {
                            if let Some(v) = labels.get(name) {
                                kept.insert(name.clone(), v.clone());
                            }
                        }
                        kept
                    }
                    None => BTreeMap::new(),
                };

                let key = key_of(&grouped);
                groups
                    .entry(key)
                    .and_modify(|(_, sum)| *sum += value)
                    .or_insert((grouped, value));
            }

            Value::Vector(groups.into_values().collect())
        }

        Expr::Binary { op, lhs, rhs } => {
            let lhs = eval(store, lhs, t);
            let rhs = eval(store, rhs, t);

            match (lhs, rhs) {
                (Value::Scalar(a), Value::Scalar(b)) => Value::Scalar(apply(*op, a, b)),
                (Value::Vector(v), Value::Scalar(s)) => {
                    Value::Vector(v.into_iter().map(|(l, x)| (l, apply(*op, x, s))).collect())
                }
                (Value::Scalar(s), Value::Vector(v)) => {
                    Value::Vector(v.into_iter().map(|(l, x)| (l, apply(*op, s, x))).collect())
                }
                (Value::Vector(a), Value::Vector(b)) => {
                    // One-to-one matching on the full label set, which is
                    // what makes `MemTotal - (MemFree + Buffers + Cached)`
                    // line up: every one of those carries the same
                    // kubernetes_node and instance.
                    let rhs_by_labels: HashMap<String, f64> =
                        b.into_iter().map(|(l, v)| (key_of(&l), v)).collect();

                    let mut out = Vec::new();
                    for (labels, value) in a {
                        if let Some(other) = rhs_by_labels.get(&key_of(&labels)) {
                            out.push((labels, apply(*op, value, *other)));
                        }
                    }
                    Value::Vector(out)
                }
            }
        }
    }
}

pub(crate) fn select<'a>(store: &'a Store, selector: &Selector) -> Vec<&'a crate::store::Series> {
    store
        .candidates(selector.name.as_deref())
        .into_iter()
        .filter(|series| {
            selector.matchers.iter().all(|matcher| {
                // Prometheus treats an absent label as the empty string,
                // which is what makes `container!=""` exclude the
                // pod-level rollups rather than everything.
                let value = series
                    .labels
                    .get(&matcher.label)
                    .map(String::as_str)
                    .unwrap_or("");
                matcher.matches(value)
            })
        })
        .collect()
}

fn without_name(labels: &Labels) -> Labels {
    let mut out = labels.clone();
    out.remove(NAME_LABEL);
    out
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
    let mut ordered: Vec<String> = Vec::new();
    let mut series: HashMap<String, RangeSeries> = HashMap::new();

    let mut t = start;
    while t <= end {
        for (labels, value) in eval(store, expr, t).into_vector() {
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
