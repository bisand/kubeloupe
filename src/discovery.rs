//! The read-only discovery endpoints: `/api/v1/labels`, `/api/v1/series`
//! and `/api/v1/label/{name}/values`.
//!
//! Lens never calls these -- it interpolates names it already has from
//! the Kubernetes API. Grafana does: its metric browser lists metrics
//! from `label/__name__/values`, and a `label_values()` template variable
//! is a call to this and nothing else. Without them a hand-written panel
//! still works, but nothing can be discovered, so every query has to be
//! typed from the README.
//!
//! This adds no collection and no state. It is a read over the label sets
//! the store already holds, which is why it costs a few kilobytes rather
//! than a feature.
//!
//! Deliberately absent: `/api/v1/status/buildinfo`. Grafana probes it to
//! decide which PromQL features to send, and a 404 makes it assume the
//! conservative subset -- which is the truth here. Answering it with a
//! version number would advertise `histogram_quantile` and subqueries
//! that [`crate::promql`] rejects.

use crate::api::{Shared, bad_data};
use crate::promql::{Expr, Selector, parser};
use crate::store::{Labels, Store};
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::collections::BTreeSet;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/v1/labels", get(labels_get).post(labels_post))
        .route("/api/v1/series", get(series_get).post(series_post))
        .route(
            "/api/v1/label/{name}/values",
            get(values_get).post(values_post),
        )
        // Prometheus serves per-metric HELP and TYPE here. Nothing is
        // recorded, and an empty map is a valid answer that stops Grafana
        // retrying a 404 on every panel refresh.
        .route("/api/v1/metadata", get(metadata))
}

/// The parameters all three share: repeated `match[]` selectors and an
/// optional time window. Both are advisory in Prometheus -- omitting
/// `match[]` means every series -- except on `/series`, where at least
/// one selector is required.
struct Args {
    selectors: Vec<Selector>,
    start: Option<i64>,
    end: Option<i64>,
}

/// `match[]` repeats, which `serde_urlencoded` (and so axum's `Query`)
/// cannot express -- it deserialises the last occurrence only. Decoding
/// the pairs directly is both correct and smaller than pulling in a
/// second query-string crate.
fn parse_args(raw: &str) -> Result<Args, String> {
    let mut args = Args {
        selectors: Vec::new(),
        start: None,
        end: None,
    };

    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "match[]" => match parser::parse(&value) {
                Ok(Expr::Selector(selector)) => args.selectors.push(selector),
                Ok(_) => return Err(format!("match[] must be a selector, found {value:?}")),
                Err(error) => return Err(format!("{error:#}")),
            },
            "start" => args.start = value.parse::<f64>().ok().map(|s| s as i64),
            "end" => args.end = value.parse::<f64>().ok().map(|s| s as i64),
            _ => {}
        }
    }

    Ok(args)
}

/// Label sets matching any selector, restricted to those with a sample
/// inside the window. The window matters: without it a node that was
/// drained yesterday keeps appearing in Grafana's variable dropdown for as
/// long as retention holds it. Unbounded, it is everything in memory, and
/// with a disk tier the last hour of that too -- a year of files is not
/// read for a dropdown.
fn matching(store: &Store, args: &Args) -> Vec<Labels> {
    let from = args.start.unwrap_or(match store.disk {
        Some(_) => crate::now() - 3600,
        None => i64::MIN,
    });
    store.label_sets(&args.selectors, from, args.end.unwrap_or(i64::MAX))
}

async fn labels(store: Shared, raw: &str) -> Response {
    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(error) => return bad_data(&error),
    };

    let store = store.read().await;
    let found = matching(&store, &args);
    let names: BTreeSet<&str> = found
        .iter()
        .flat_map(|labels| labels.keys().map(String::as_str))
        .collect();

    success(json!(names))
}

async fn values(store: Shared, name: &str, raw: &str) -> Response {
    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(error) => return bad_data(&error),
    };

    let store = store.read().await;
    let found = matching(&store, &args);
    let values: BTreeSet<&str> = found
        .iter()
        .filter_map(|labels| labels.get(name).map(String::as_str))
        .collect();

    success(json!(values))
}

async fn series(store: Shared, raw: &str) -> Response {
    let args = match parse_args(raw) {
        Ok(args) => args,
        Err(error) => return bad_data(&error),
    };

    // Prometheus rejects a bare /series rather than serialising every
    // label set it holds, and Grafana always sends one.
    if args.selectors.is_empty() {
        return bad_data("at least one match[] argument is required");
    }

    let store = store.read().await;
    success(json!(matching(&store, &args)))
}

async fn metadata() -> Response {
    success(json!({}))
}

fn success(data: serde_json::Value) -> Response {
    Json(json!({ "status": "success", "data": data })).into_response()
}

async fn labels_get(State(store): State<Shared>, RawQuery(raw): RawQuery) -> Response {
    labels(store, raw.as_deref().unwrap_or_default()).await
}

async fn labels_post(State(store): State<Shared>, body: String) -> Response {
    labels(store, &body).await
}

async fn series_get(State(store): State<Shared>, RawQuery(raw): RawQuery) -> Response {
    series(store, raw.as_deref().unwrap_or_default()).await
}

async fn series_post(State(store): State<Shared>, body: String) -> Response {
    series(store, &body).await
}

async fn values_get(
    State(store): State<Shared>,
    Path(name): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    values(store, &name, raw.as_deref().unwrap_or_default()).await
}

async fn values_post(
    State(store): State<Shared>,
    Path(name): Path<String>,
    body: String,
) -> Response {
    values(store, &name, &body).await
}
