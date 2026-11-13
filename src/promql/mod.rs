//! A deliberately small PromQL implementation: the grammar Lens
//! generates, plus what a hand-written Grafana dashboard needs, and
//! nothing else.
//!
//! Lens builds every query from one `switch` statement -- see
//! `lens-provider.injectable.ts` in the Freelens source -- interpolating
//! only node, pod and namespace names. That makes its grammar finite:
//!
//!   selectors        node_memory_MemTotal_bytes{kubernetes_node=~"a|b"}
//!                    {__name__=~"kubelet_running_pods|...", instance=~"n"}
//!   rate over range  rate(node_cpu_seconds_total{...}[1m])
//!   aggregation      sum(...) / sum(...) by (pod, namespace)
//!   arithmetic       A{...} - (B{...} + C{...})
//!
//! On top of that, for dashboards: `avg`, `min`, `max`, `count`,
//! `without`, `increase`, `irate`, comparisons (`== 1`, `> 0`), `and`,
//! `or`, `unless`, `clamp_min`, `clamp_max`, `vector()`, `time()` and
//! scientific numbers (`1e6`). Absent on purpose: `offset`, `on()` and
//! `group_left`, subqueries and histograms.
//!
//! Parsing this structurally rather than pattern-matching the query
//! strings is the difference between surviving a Lens update that
//! reorders a label and going silently blank.

pub mod eval;
pub mod parser;

use regex_lite::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOp {
    Eq,
    Ne,
    Re,
    NotRe,
}

#[derive(Debug)]
pub struct Matcher {
    pub label: String,
    pub op: MatchOp,
    pub value: String,
    /// Compiled once at parse time for `=~` and `!~`.
    pub regex: Option<Regex>,
}

impl Matcher {
    pub fn matches(&self, value: &str) -> bool {
        match self.op {
            MatchOp::Eq => value == self.value,
            MatchOp::Ne => value != self.value,
            // Prometheus anchors regex matchers at both ends; an
            // unanchored match would let `pod=~"web"` select `web-2` and
            // quietly double a sum.
            MatchOp::Re => self.regex.as_ref().is_some_and(|r| r.is_match(value)),
            MatchOp::NotRe => !self.regex.as_ref().is_some_and(|r| r.is_match(value)),
        }
    }
}

#[derive(Debug)]
pub struct Selector {
    /// Present when the metric name was written outside the braces, which
    /// lets the store skip straight to that bucket.
    pub name: Option<String>,
    pub matchers: Vec<Matcher>,
    /// Numbered in parse order, so the samples fetched for a query can be
    /// looked up by selector without walking the tree twice in step.
    pub id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// Comparisons filter: a series stays, with its own value, where the
    /// comparison holds.
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    And,
    Or,
    Unless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeFn {
    Rate,
    Irate,
    Increase,
}

#[derive(Debug)]
pub enum Expr {
    Selector(Selector),
    /// `rate(<selector>[<window>])` and kin, window in seconds.
    Range {
        func: RangeFn,
        selector: Selector,
        window: i64,
    },
    Agg {
        op: AggOp,
        expr: Box<Expr>,
        /// `None` means no clause: everything folds into one series.
        by: Option<Vec<String>>,
        /// `without (...)` rather than `by (...)`.
        without: bool,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `clamp_min(expr, bound)` (min) and `clamp_max` (not min).
    Clamp {
        expr: Box<Expr>,
        bound: Box<Expr>,
        min: bool,
    },
    /// `vector(scalar)`: one series with no labels.
    Vector(Box<Expr>),
    /// `time()`: the evaluation instant.
    Time,
    Number(f64),
}
