//! The Prometheus HTTP API, restricted to what Lens calls.
//!
//! Lens POSTs form-urlencoded by default (see `get-metrics.injectable.ts`)
//! and only falls back to GET when the cluster preference says so, so both
//! are wired up. A malformed query answers 422, which is the status
//! Prometheus uses and the one Lens logs without retrying -- returning 500
//! instead would earn five retries per broken chart.

use crate::promql::{eval, parser};
use crate::store::Store;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type Shared = Arc<RwLock<Store>>;

#[derive(Deserialize)]
pub struct Params {
    query: String,
    start: Option<String>,
    end: Option<String>,
    step: Option<String>,
}

pub fn router(store: Shared) -> Router {
    Router::new()
        .route("/api/v1/query_range", get(range_get).post(range_post))
        .route("/api/v1/query", get(instant_get).post(instant_post))
        .route("/health", get(health))
        .route("/-/stats", get(stats))
        .with_state(store)
}

async fn health() -> &'static str {
    "ok"
}

async fn stats(State(store): State<Shared>) -> Json<serde_json::Value> {
    let store = store.read().await;
    Json(json!({
        "series": store.series_count(),
        "samples": store.sample_count(),
    }))
}

async fn range_get(State(store): State<Shared>, Query(params): Query<Params>) -> Response {
    range(store, params).await
}

async fn range_post(State(store): State<Shared>, Form(params): Form<Params>) -> Response {
    range(store, params).await
}

async fn instant_get(State(store): State<Shared>, Query(params): Query<Params>) -> Response {
    instant(store, params).await
}

async fn instant_post(State(store): State<Shared>, Form(params): Form<Params>) -> Response {
    instant(store, params).await
}

/// Prometheus caps a range query at 11000 points. Lens never asks for
/// more than a few hundred, so this only ever stops a hand-typed query
/// from pinning the one core this node has.
const MAX_POINTS: i64 = 11_000;

async fn range(store: Shared, params: Params) -> Response {
    let expr = match parser::parse(&params.query) {
        Ok(expr) => expr,
        Err(error) => return bad_data(&format!("{error:#}")),
    };

    let now = crate::now();
    let end = params.end.as_deref().and_then(parse_time).unwrap_or(now);
    let start = params
        .start
        .as_deref()
        .and_then(parse_time)
        .unwrap_or(end - 3600);
    let step = params
        .step
        .as_deref()
        .and_then(parse_time)
        .unwrap_or(60)
        .max(1);

    if start > end {
        return bad_data("start is after end");
    }
    if (end - start) / step > MAX_POINTS {
        return bad_data("exceeded maximum resolution of 11000 points per timeseries");
    }

    let store = store.read().await;
    let series = eval::query_range(&store, &expr, start, end, step);

    let result: Vec<serde_json::Value> = series
        .into_iter()
        .map(|s| {
            let values: Vec<serde_json::Value> = s
                .values
                .into_iter()
                .map(|(t, v)| json!([t, format_value(v)]))
                .collect();
            json!({ "metric": s.labels, "values": values })
        })
        .collect();

    success("matrix", result)
}

async fn instant(store: Shared, params: Params) -> Response {
    let expr = match parser::parse(&params.query) {
        Ok(expr) => expr,
        Err(error) => return bad_data(&format!("{error:#}")),
    };

    let at = params
        .start
        .as_deref()
        .and_then(parse_time)
        .unwrap_or_else(crate::now);
    let store = store.read().await;

    let result: Vec<serde_json::Value> = match eval::eval(&store, &expr, at) {
        eval::Value::Vector(v) => v
            .into_iter()
            .map(|(labels, value)| json!({ "metric": labels, "value": [at, format_value(value)] }))
            .collect(),
        eval::Value::Scalar(value) => {
            return success_scalar(json!([at, format_value(value)]));
        }
    };

    success("vector", result)
}

fn parse_time(text: &str) -> Option<i64> {
    text.parse::<f64>().ok().map(|seconds| seconds as i64)
}

/// Prometheus renders sample values as strings, and Lens parses them back
/// with parseFloat. `1e9` style output is valid for both.
fn format_value(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn success(result_type: &str, result: Vec<serde_json::Value>) -> Response {
    Json(json!({
        "status": "success",
        "data": { "resultType": result_type, "result": result },
    }))
    .into_response()
}

fn success_scalar(value: serde_json::Value) -> Response {
    Json(json!({
        "status": "success",
        "data": { "resultType": "scalar", "result": value },
    }))
    .into_response()
}

fn bad_data(message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "status": "error", "errorType": "bad_data", "error": message })),
    )
        .into_response()
}
