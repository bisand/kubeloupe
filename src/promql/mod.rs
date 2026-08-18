//! A deliberately small PromQL implementation: exactly the grammar Lens
//! generates, and nothing else.
//!
//! Lens builds every query from one `switch` statement -- see
//! `lens-provider.injectable.ts` in the Freelens source -- interpolating
//! only node, pod and namespace names. That makes the grammar finite:
//!
//!   selectors        node_memory_MemTotal_bytes{kubernetes_node=~"a|b"}
//!                    {__name__=~"kubelet_running_pods|...", instance=~"n"}
//!   rate over range  rate(node_cpu_seconds_total{...}[1m])
//!   aggregation      sum(...) / sum(...) by (pod, namespace)
//!   arithmetic       A{...} - (B{...} + C{...})
//!
//! Parsing this structurally rather than pattern-matching the ~40 query
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug)]
pub enum Expr {
    Selector(Selector),
    /// `rate(<selector>[<window>])`, window in seconds.
    Rate(Selector, i64),
    Sum {
        expr: Box<Expr>,
        /// `None` means no `by` clause: everything folds into one series.
        by: Option<Vec<String>>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Number(f64),
}
