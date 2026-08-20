//! The queries under test are the ones Lens actually sends.
//!
//! Each string below is what `getLensLikeQueryFor` produces once the node,
//! pod and namespace names are interpolated -- copied from the shape in
//! the Freelens source rather than paraphrased. If a Lens upgrade changes
//! them, these are what should fail.

use crate::promql::{eval, parser};
use crate::store::{Store, labels};

const NODE: &str = "node-a";
const STEP: i64 = 30;
const FIRST: i64 = 1_000;
const LAST: i64 = 1_180; // seven samples

/// Appends a series over the whole window: `base + increment * n`, so a
/// counter can be given a known rate.
fn series(store: &mut Store, name: &str, pairs: &[(&str, &str)], base: f64, increment: f64) {
    let mut t = FIRST;
    let mut n = 0.0;
    while t <= LAST {
        store.append(labels(name, pairs), t, base + increment * n);
        t += STEP;
        n += 1.0;
    }
}

fn fixture() -> Store {
    let mut store = Store::new(86_400);
    let node_labels = [
        ("kubernetes_node", NODE),
        ("instance", NODE),
        ("node", NODE),
    ];

    // 2000 total, 500 free, nothing in buffers or cache -> 1500 used.
    series(
        &mut store,
        "node_memory_MemTotal_bytes",
        &node_labels,
        2000.0,
        0.0,
    );
    series(
        &mut store,
        "node_memory_MemFree_bytes",
        &node_labels,
        500.0,
        0.0,
    );
    series(
        &mut store,
        "node_memory_Buffers_bytes",
        &node_labels,
        0.0,
        0.0,
    );
    series(
        &mut store,
        "node_memory_Cached_bytes",
        &node_labels,
        0.0,
        0.0,
    );

    // 15 core-seconds per 30s tick -> half a core.
    series(
        &mut store,
        "node_cpu_seconds_total",
        &[
            ("kubernetes_node", NODE),
            ("instance", NODE),
            ("node", NODE),
            ("mode", "user"),
            ("cpu", "0"),
        ],
        100.0,
        15.0,
    );

    series(
        &mut store,
        "node_filesystem_size_bytes",
        &[
            ("kubernetes_node", NODE),
            ("instance", NODE),
            ("node", NODE),
            ("mountpoint", "/"),
        ],
        100.0,
        0.0,
    );
    series(
        &mut store,
        "node_filesystem_avail_bytes",
        &[
            ("kubernetes_node", NODE),
            ("instance", NODE),
            ("node", NODE),
            ("mountpoint", "/"),
        ],
        40.0,
        0.0,
    );

    series(
        &mut store,
        "kubelet_running_pods",
        &[("instance", NODE)],
        12.0,
        0.0,
    );

    for (resource, unit, value) in [
        ("memory", "byte", 2000.0),
        ("cpu", "core", 1.0),
        ("pods", "integer", 110.0),
    ] {
        series(
            &mut store,
            "kube_node_status_capacity",
            &[("node", NODE), ("resource", resource), ("unit", unit)],
            value,
            0.0,
        );
    }

    series(
        &mut store,
        "kube_pod_container_resource_requests",
        &[
            ("namespace", "app"),
            ("pod", "api-1"),
            ("container", "api"),
            ("node", NODE),
            ("resource", "memory"),
            ("unit", "byte"),
        ],
        256.0,
        0.0,
    );

    let container = [
        ("namespace", "app"),
        ("pod", "api-1"),
        ("container", "api"),
        ("image", "registry.example/api:1"),
        ("instance", NODE),
    ];
    // 3 core-seconds per tick -> 0.1 cores.
    series(
        &mut store,
        "container_cpu_usage_seconds_total",
        &container,
        10.0,
        3.0,
    );
    series(
        &mut store,
        "container_memory_working_set_bytes",
        &container,
        128.0,
        0.0,
    );

    // 300 bytes per tick -> 10 B/s.
    series(
        &mut store,
        "container_network_receive_bytes_total",
        &[("namespace", "app"), ("pod", "api-1"), ("instance", NODE)],
        0.0,
        300.0,
    );

    series(
        &mut store,
        "kubelet_volume_stats_used_bytes",
        &[("persistentvolumeclaim", "data"), ("namespace", "app")],
        42.0,
        0.0,
    );

    store
}

/// Evaluates at the last sample and returns the single expected series.
fn one(store: &Store, query: &str) -> f64 {
    let expr = parser::parse(query).unwrap_or_else(|e| panic!("parse {query:?}: {e:#}"));
    match eval::eval(store, &expr, LAST) {
        eval::Value::Vector(mut v) => {
            assert_eq!(
                v.len(),
                1,
                "expected one series from {query:?}, got {}",
                v.len()
            );
            v.remove(0).1
        }
        eval::Value::Scalar(_) => panic!("expected a vector from {query:?}"),
    }
}

#[test]
fn cluster_memory_usage_is_total_minus_free_buffers_and_cached() {
    let store = fixture();
    // The `by (kubernetes_name)` is Lens', and nothing carries that label:
    // every series folds into one, which is what the chart wants.
    let value = one(
        &store,
        r#"sum(node_memory_MemTotal_bytes{kubernetes_node=~"node-a"} - (node_memory_MemFree_bytes{kubernetes_node=~"node-a"} + node_memory_Buffers_bytes{kubernetes_node=~"node-a"} + node_memory_Cached_bytes{kubernetes_node=~"node-a"})) by (kubernetes_name)"#,
    );
    assert_eq!(value, 1500.0);
}

#[test]
fn cluster_cpu_usage_is_a_rate_over_the_counter() {
    let store = fixture();
    let value = one(
        &store,
        r#"sum(rate(node_cpu_seconds_total{kubernetes_node=~"node-a", mode=~"user|system"}[1m]))"#,
    );
    assert!(
        (value - 0.5).abs() < 1e-9,
        "expected half a core, got {value}"
    );
}

#[test]
fn pod_usage_matches_on_a_name_regex_alone() {
    let store = fixture();
    // A selector with no metric name outside the braces: the store cannot
    // use its index and has to scan, which is worth having a test for.
    let value = one(
        &store,
        r#"sum({__name__=~"kubelet_running_pod_count|kubelet_running_pods", instance=~"node-a"})"#,
    );
    assert_eq!(value, 12.0);
}

#[test]
fn node_capacity_and_requests_select_by_resource() {
    let store = fixture();
    assert_eq!(
        one(
            &store,
            r#"sum(kube_node_status_capacity{node=~"node-a", resource="memory"}) by (component)"#
        ),
        2000.0
    );
    assert_eq!(
        one(
            &store,
            r#"sum(kube_node_status_capacity{node=~"node-a", resource="cpu"}) by (component)"#
        ),
        1.0
    );
    assert_eq!(
        one(
            &store,
            r#"sum(kube_pod_container_resource_requests{node=~"node-a", resource="memory"}) by (component)"#
        ),
        256.0
    );
}

#[test]
fn node_filesystem_usage_subtracts_available_from_size() {
    let store = fixture();
    let value = one(
        &store,
        r#"sum(node_filesystem_size_bytes{mountpoint=~"/|/local"} - node_filesystem_avail_bytes{mountpoint=~"/|/local"}) by (kubernetes_node)"#,
    );
    assert_eq!(value, 60.0);
}

#[test]
fn pod_cpu_memory_and_network_group_by_the_requested_selector() {
    let store = fixture();
    let cpu = one(
        &store,
        r#"sum(rate(container_cpu_usage_seconds_total{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}[1m])) by (pod)"#,
    );
    assert!((cpu - 0.1).abs() < 1e-9, "expected 0.1 cores, got {cpu}");

    assert_eq!(
        one(
            &store,
            r#"sum(container_memory_working_set_bytes{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}) by (pod)"#
        ),
        128.0
    );

    let rx = one(
        &store,
        r#"sum(rate(container_network_receive_bytes_total{pod=~"api-1",namespace="app"}[1m])) by (pod)"#,
    );
    assert!((rx - 10.0).abs() < 1e-9, "expected 10 B/s, got {rx}");
}

#[test]
fn pvc_usage_groups_by_claim_and_namespace() {
    let store = fixture();
    let expr = parser::parse(
        r#"sum(kubelet_volume_stats_used_bytes{persistentvolumeclaim="data",namespace="app"}) by (persistentvolumeclaim, namespace)"#,
    )
    .unwrap();
    match eval::eval(&store, &expr, LAST) {
        eval::Value::Vector(v) => {
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].1, 42.0);
            assert_eq!(
                v[0].0.get("persistentvolumeclaim").map(String::as_str),
                Some("data")
            );
            assert_eq!(v[0].0.get("namespace").map(String::as_str), Some("app"));
        }
        eval::Value::Scalar(_) => panic!("expected a vector"),
    }
}

#[test]
fn an_anchored_regex_does_not_match_a_longer_pod_name() {
    let mut store = fixture();
    let other = [
        ("namespace", "app"),
        ("pod", "api-10"),
        ("container", "api"),
        ("image", "registry.example/api:1"),
        ("instance", NODE),
    ];
    series(
        &mut store,
        "container_memory_working_set_bytes",
        &other,
        999.0,
        0.0,
    );

    // Unanchored, `pod=~"api-1"` would also select api-10 and the chart
    // would silently show 1127 instead of 128.
    let value = one(
        &store,
        r#"sum(container_memory_working_set_bytes{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}) by (pod)"#,
    );
    assert_eq!(value, 128.0);
}

#[test]
fn an_empty_image_label_is_excluded_the_way_lens_expects() {
    let mut store = Store::new(86_400);
    // A series whose image could not be resolved must not be published
    // with an empty label: Lens filters `image!=""` and would drop it.
    series(
        &mut store,
        "container_memory_working_set_bytes",
        &[
            ("namespace", "app"),
            ("pod", "api-1"),
            ("container", "api"),
            ("image", ""),
        ],
        64.0,
        0.0,
    );
    let expr = parser::parse(
        r#"sum(container_memory_working_set_bytes{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}) by (pod)"#,
    )
    .unwrap();
    match eval::eval(&store, &expr, LAST) {
        eval::Value::Vector(v) => assert!(v.is_empty(), "an empty image label should not match"),
        eval::Value::Scalar(_) => panic!("expected a vector"),
    }
}

#[test]
fn a_counter_reset_does_not_produce_a_negative_rate() {
    let mut store = Store::new(86_400);
    let container = [
        ("namespace", "app"),
        ("pod", "api-1"),
        ("container", "api"),
        ("image", "x"),
    ];
    // A restart mid-window: the counter drops back to nearly zero.
    for (t, v) in [
        (FIRST, 100.0),
        (FIRST + 30, 130.0),
        (FIRST + 60, 5.0),
        (FIRST + 90, 35.0),
    ] {
        store.append(
            labels("container_cpu_usage_seconds_total", &container),
            t,
            v,
        );
    }
    let expr = parser::parse(
        r#"sum(rate(container_cpu_usage_seconds_total{container!="",image!=""}[1m])) by (pod)"#,
    )
    .unwrap();
    match eval::eval(&store, &expr, FIRST + 90) {
        eval::Value::Vector(v) => {
            assert_eq!(v.len(), 1);
            assert!(
                v[0].1 > 0.0,
                "a reset should not read as a negative rate: {}",
                v[0].1
            );
        }
        eval::Value::Scalar(_) => panic!("expected a vector"),
    }
}

#[test]
fn a_stale_series_stops_answering_rather_than_flatlining() {
    let store = fixture();
    // Ten minutes past the last sample is beyond the 5m lookback.
    let expr = parser::parse(r#"sum(kubelet_running_pods{instance=~"node-a"})"#).unwrap();
    match eval::eval(&store, &expr, LAST + 600) {
        eval::Value::Vector(v) => assert!(v.is_empty(), "expected no points past the lookback"),
        eval::Value::Scalar(_) => panic!("expected a vector"),
    }
}

#[test]
fn query_range_returns_a_point_per_step() {
    let store = fixture();
    let expr = parser::parse(r#"sum(kubelet_running_pods{instance=~"node-a"})"#).unwrap();
    let series = eval::query_range(&store, &expr, FIRST, LAST, 60);
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].values.len(), 4); // 1000, 1060, 1120, 1180
    assert!(series[0].values.iter().all(|(_, v)| *v == 12.0));
}

#[test]
fn every_lens_query_template_parses() {
    // Every arm of getLensLikeQueryFor, including the ingress ones this
    // cluster has no controller for -- a parse error there would still
    // reach Lens as a 422.
    let queries = [
        r#"sum(container_memory_working_set_bytes{container!="POD",container!="",image!="",instance=~"node-a"}) by (component)"#,
        r#"sum(kube_pod_container_resource_limits{node=~"node-a", resource="memory"}) by (component)"#,
        r#"sum(kube_node_status_allocatable{node=~"node-a", resource="memory"}) by (component)"#,
        r#"sum(rate(node_cpu_seconds_total{kubernetes_node=~"node-a", mode=~"user|system"}[1m]))"#,
        r#"sum(kube_node_status_capacity{node=~"node-a", resource="pods"}) by (component)"#,
        r#"sum(node_filesystem_size_bytes{kubernetes_node=~"node-a", mountpoint=~"/|/local"}) by (kubernetes_node)"#,
        r#"sum (node_memory_MemTotal_bytes - (node_memory_MemFree_bytes + node_memory_Buffers_bytes + node_memory_Cached_bytes)) by (kubernetes_node)"#,
        r#"sum(container_memory_working_set_bytes{container!="POD",container!=""}) by (instance)"#,
        r#"sum(kube_node_status_allocatable{resource="cpu"}) by (node)"#,
        r#"sum(rate(container_cpu_usage_seconds_total{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}[1m])) by (pod, namespace)"#,
        r#"sum(container_fs_usage_bytes{container!="POD",container!="",image!="",pod=~"api-1",namespace="app"}) by (pod)"#,
        r#"sum(rate(container_fs_writes_bytes_total{container!="",image!="", pod=~"api-1", namespace="app"}[1m])) by (pod)"#,
        r#"sum(rate(container_network_transmit_bytes_total{pod=~"api-1",namespace="app"}[1m])) by (pod)"#,
        r#"sum(kubelet_volume_stats_capacity_bytes{persistentvolumeclaim="data",namespace="app"}) by (persistentvolumeclaim, namespace)"#,
        r#"sum(rate(nginx_ingress_controller_bytes_sent_sum{ingress="web",namespace="app",status=~"^2\\d*"}[1m])) by (ingress, namespace)"#,
        r#"sum(rate(nginx_ingress_controller_request_duration_seconds_sum{ingress="web",namespace="app"}[1m])) by (ingress, namespace)"#,
    ];

    for query in queries {
        parser::parse(query).unwrap_or_else(|e| panic!("failed to parse {query:?}: {e:#}"));
    }
}

#[test]
fn kubernetes_quantities_parse() {
    use crate::kube::parse_quantity;
    assert_eq!(parse_quantity("1"), Some(1.0));
    assert_eq!(parse_quantity("100m"), Some(0.1));
    assert_eq!(parse_quantity("2Gi"), Some(2.0 * 1024.0 * 1024.0 * 1024.0));
    assert_eq!(parse_quantity("1990168Ki"), Some(1_990_168.0 * 1024.0));
    assert_eq!(parse_quantity("500M"), Some(500e6));
    assert_eq!(parse_quantity("110"), Some(110.0));
    assert_eq!(parse_quantity(""), None);
}

// --- boundedness ------------------------------------------------------------
//
// The store is memory-only and never touches disk, so "does it grow without
// limit" is the whole safety question. These assert the two ways it could.

#[test]
fn the_store_is_bounded_by_retention_however_long_it_runs() {
    const RETENTION: i64 = 3_600; // 1h
    const INTERVAL: i64 = 30;
    let mut store = Store::new(RETENTION);
    let container = [
        ("namespace", "app"),
        ("pod", "api-1"),
        ("container", "api"),
        ("image", "x"),
    ];

    // Ten retention windows of continuous collection.
    let mut peak = 0;
    let mut t = 0;
    while t < RETENTION * 10 {
        store.append(
            labels("container_memory_working_set_bytes", &container),
            t,
            1.0,
        );
        store.append(
            labels("kubelet_running_pods", &[("instance", NODE)]),
            t,
            5.0,
        );
        store.prune(t);
        peak = peak.max(store.sample_count());
        t += INTERVAL;
    }

    // Two series, each holding at most one retention window plus the
    // overhang of a sealed chunk. Retention drops chunks whole rather
    // than rewriting them, so the oldest one lingers until its newest
    // sample ages out; `MAX_CHUNK_SPAN_FRACTION` is what bounds that.
    let window = RETENTION / INTERVAL + 1;
    let overhang = RETENTION / crate::store::MAX_CHUNK_SPAN_FRACTION / INTERVAL + 1;
    let ceiling = 2 * (window + overhang) as usize;
    assert_eq!(store.series_count(), 2);
    assert!(
        peak <= ceiling,
        "peak {peak} samples exceeded the retention ceiling of {ceiling}"
    );
    // And it really did fill up rather than staying trivially small: a
    // full retention window of both series, at minimum.
    assert!(
        peak >= 2 * window as usize,
        "expected the window to fill, peaked at {peak}"
    );
}

#[test]
fn series_that_stop_reporting_are_dropped_entirely() {
    const RETENTION: i64 = 600;
    let mut store = Store::new(RETENTION);

    // A pod that exists briefly, as happens on every rollout.
    for t in (0..300).step_by(30) {
        store.append(
            labels(
                "container_memory_working_set_bytes",
                &[
                    ("namespace", "app"),
                    ("pod", "doomed-1"),
                    ("container", "api"),
                    ("image", "x"),
                ],
            ),
            t,
            1.0,
        );
    }
    assert_eq!(store.series_count(), 1);

    // Long after its last sample, the series -- and its name index entry --
    // must be gone, not just empty. Churning pods would otherwise
    // accumulate for the life of the process.
    store.prune(1_000);
    assert_eq!(
        store.series_count(),
        0,
        "an aged-out series should be removed"
    );
    assert_eq!(store.sample_count(), 0);

    // The name index must have been cleaned too: re-appending has to
    // produce exactly one series, not attach to a stale key.
    store.append(
        labels("kubelet_running_pods", &[("instance", NODE)]),
        1_030,
        1.0,
    );
    assert_eq!(store.series_count(), 1);
}

// --- persistence ------------------------------------------------------------

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("kubeloupe-test-{name}-{}", std::process::id()));
    p.push("snapshot.bin");
    p
}

#[test]
fn a_snapshot_round_trips_every_series_and_sample() {
    let path = temp_path("roundtrip");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let original = fixture();

    crate::snapshot::save(&original, &path).expect("save");
    let restored = crate::snapshot::load(&path, 86_400, LAST).expect("load");

    assert_eq!(restored.series_count(), original.series_count());
    assert_eq!(restored.sample_count(), original.sample_count());

    // The real check is not the counts but that queries still answer the
    // same, which exercises labels, ordering and the name index together.
    for query in [
        r#"sum(kubelet_running_pods{instance=~"node-a"})"#,
        r#"sum(node_memory_MemTotal_bytes{kubernetes_node=~"node-a"} - (node_memory_MemFree_bytes{kubernetes_node=~"node-a"} + node_memory_Buffers_bytes{kubernetes_node=~"node-a"} + node_memory_Cached_bytes{kubernetes_node=~"node-a"})) by (kubernetes_name)"#,
        r#"sum(rate(node_cpu_seconds_total{kubernetes_node=~"node-a", mode=~"user|system"}[1m]))"#,
    ] {
        assert_eq!(
            one(&original, query),
            one(&restored, query),
            "differs for {query}"
        );
    }

    // The temporary file must not survive a successful save.
    assert!(
        !path.with_extension("tmp").exists(),
        "a .tmp file was left behind"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn loading_drops_samples_that_aged_out_while_the_process_was_down() {
    let path = temp_path("stale");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    crate::snapshot::save(&fixture(), &path).expect("save");

    // Back up an hour later, with a ten minute retention: everything in
    // the snapshot is already history and none of it should return.
    let restored = crate::snapshot::load(&path, 600, LAST + 3_600).expect("load");
    assert_eq!(restored.series_count(), 0);
    assert_eq!(restored.sample_count(), 0);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_corrupt_or_truncated_snapshot_is_an_error_never_a_panic() {
    let dir = temp_path("corrupt");
    let dir = dir.parent().unwrap().to_path_buf();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let good = dir.join("good.bin");
    crate::snapshot::save(&fixture(), &good).expect("save");
    let bytes = std::fs::read(&good).unwrap();

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("wrong magic", b"XXXX\x01\x00\x00\x00\x00\x00".to_vec()),
        ("header only", bytes[..6].to_vec()),
        // The nasty one: a write interrupted by an OOM kill. Without the
        // temp-and-rename in save() this is what the live file would be.
        ("truncated mid-series", bytes[..bytes.len() / 2].to_vec()),
        ("garbage", vec![0xff; 512]),
    ];

    for (name, content) in cases {
        let path = dir.join(format!("{}.bin", name.replace(' ', "-")));
        std::fs::write(&path, &content).unwrap();
        let result = crate::snapshot::load(&path, 86_400, LAST);
        assert!(
            result.is_err(),
            "{name} should have failed to load, not succeeded"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_snapshot_overwrites_the_previous_one_in_place() {
    let path = temp_path("overwrite");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    crate::snapshot::save(&fixture(), &path).expect("first save");
    let first = std::fs::metadata(&path).unwrap().len();

    let mut bigger = fixture();
    for n in 0..50 {
        series(
            &mut bigger,
            "container_memory_working_set_bytes",
            &[
                ("namespace", "app"),
                ("pod", &format!("extra-{n}")),
                ("container", "api"),
                ("image", "x"),
            ],
            1.0,
            0.0,
        );
    }
    crate::snapshot::save(&bigger, &path).expect("second save");

    assert!(std::fs::metadata(&path).unwrap().len() > first);
    let restored = crate::snapshot::load(&path, 86_400, LAST).expect("load");
    assert_eq!(restored.series_count(), bigger.series_count());

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// -- discovery ---------------------------------------------------------
//
// Lens never calls these. Grafana's metric browser is one call to
// `label/__name__/values`, and a `label_values(...)` variable is one call
// to `label/{name}/values` -- so these tests are the whole of what makes
// this readable from something other than Lens.

fn app() -> axum::Router {
    crate::api::router(std::sync::Arc::new(tokio::sync::RwLock::new(fixture())))
}

async fn get(uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;

    let response = app()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn the_metric_browser_can_list_every_metric_name() {
    let (status, body) = get("/api/v1/label/__name__/values").await;

    assert_eq!(status, 200);
    let names = body["data"].as_array().unwrap();
    assert!(names.contains(&serde_json::json!("node_memory_MemTotal_bytes")));
    assert!(names.contains(&serde_json::json!("container_cpu_usage_seconds_total")));
}

#[tokio::test]
async fn a_label_values_variable_narrows_to_the_selector_it_was_given() {
    // What `label_values(kube_pod_container_resource_requests, pod)`
    // sends. Without the match[] it would return every pod in the store,
    // which is the failure mode where a Grafana dropdown quietly lists
    // pods that have no such series.
    let (status, body) =
        get("/api/v1/label/pod/values?match%5B%5D=kube_pod_container_resource_requests").await;

    assert_eq!(status, 200);
    let pods = body["data"].as_array().unwrap();
    assert!(!pods.is_empty());

    let (_, all) = get("/api/v1/label/pod/values").await;
    assert!(all["data"].as_array().unwrap().len() >= pods.len());
}

#[tokio::test]
async fn labels_lists_the_label_names_and_series_needs_a_selector() {
    let (status, body) = get("/api/v1/labels").await;
    assert_eq!(status, 200);
    let names = body["data"].as_array().unwrap();
    assert!(names.contains(&serde_json::json!("kubernetes_node")));
    assert!(names.contains(&serde_json::json!("__name__")));

    // Prometheus rejects a bare /series rather than serialising the whole
    // store, and 422 is what the rest of this API answers for bad_data.
    let (status, _) = get("/api/v1/series").await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn a_series_that_stopped_before_the_window_is_not_offered() {
    // The fixture ends at LAST. A dashboard asking about now would
    // otherwise keep offering a node drained a day ago for as long as
    // retention holds its samples.
    let (status, body) = get(&format!(
        "/api/v1/series?match%5B%5D=node_memory_MemTotal_bytes&start={}",
        LAST + 1
    ))
    .await;

    assert_eq!(status, 200);
    assert!(body["data"].as_array().unwrap().is_empty());

    let (_, body) = get(&format!(
        "/api/v1/series?match%5B%5D=node_memory_MemTotal_bytes&start={}&end={}",
        FIRST, LAST
    ))
    .await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["kubernetes_node"], NODE);
}

#[tokio::test]
async fn a_malformed_match_argument_is_bad_data_not_a_panic() {
    let (status, body) = get("/api/v1/labels?match%5B%5D=sum(rate(x%5B1m%5D))").await;

    assert_eq!(status, 422);
    assert_eq!(body["errorType"], "bad_data");
}

// -- the Helm grammar ---------------------------------------------------
//
// Lens' own provider is `isConfigurable: false`, so Lens will only ever
// look for it at `lens-metrics/prometheus`. The Helm provider is
// configurable -- it takes an explicit service address -- and its queries
// differ from Lens' in eight of forty, every one of them a label rename
// rather than a different shape. These assert the renamed ones, so the
// daemon can be addressed anywhere and still answer.

#[test]
fn helm_queries_group_nodes_by_node_rather_than_kubernetes_node() {
    let store = fixture();

    // `by (component)` where Lens says `by (kubernetes_name)`: neither
    // label exists here, so both fold to a single cluster-wide series.
    assert_eq!(
        one(
            &store,
            "sum(node_memory_MemTotal_bytes - (node_memory_MemFree_bytes + node_memory_Buffers_bytes + node_memory_Cached_bytes)) by (component)"
        ),
        1500.0
    );

    // Selecting and grouping on `node`, which only works because the
    // series now carries it alongside `kubernetes_node`.
    assert_eq!(
        one(
            &store,
            "sum(node_filesystem_size_bytes{node=~\"node-a\", mountpoint=~\"/\"}) by (node)"
        ),
        100.0
    );
    assert_eq!(
        one(
            &store,
            "sum(node_memory_MemTotal_bytes - (node_memory_MemFree_bytes + node_memory_Buffers_bytes + node_memory_Cached_bytes)) by (node)"
        ),
        1500.0
    );
}

#[test]
fn helm_node_cpu_reads_the_same_counter_over_a_five_minute_window() {
    let store = fixture();

    // Helm's rate accuracy is 5m against Lens' 1m. The fixture spans
    // three minutes, so a 5m window covers all of it and the rate is the
    // same underlying counter either way.
    let lens = one(
        &store,
        "sum(rate(node_cpu_seconds_total{kubernetes_node=~\"node-a\", mode=~\"user|system\"}[1m]))",
    );
    let helm = one(
        &store,
        "sum(rate(node_cpu_seconds_total{node=~\"node-a\", mode=~\"user|system\"}[5m]))",
    );
    assert!(lens > 0.0, "the Lens form must produce a rate at all");
    assert!(
        (helm - lens).abs() < 1e-9,
        "helm {helm} and lens {lens} read the same counter"
    );

    assert!(
        one(
            &store,
            "sum(rate(node_cpu_seconds_total{mode=~\"user|system\"}[5m])) by(node)"
        ) > 0.0
    );
}

// --- compression ------------------------------------------------------------

/// Every shape the collector actually produces, plus the awkward ones that
/// have to fall back to raw floats.
fn sample_shapes() -> Vec<(&'static str, Vec<f64>)> {
    let mut out: Vec<(&'static str, Vec<f64>)> = vec![("constant", vec![4.0; 300])];

    out.push(("zero", vec![0.0; 300]));
    // A memory gauge: whole bytes, wandering.
    out.push((
        "gauge",
        (0..300)
            .map(|i: i64| (536_870_912 + i * 4_096 - (i % 7) * 8_192) as f64)
            .collect(),
    ));
    // A CPU counter in nanoseconds divided into seconds, which is what
    // makes the integer path worth having.
    out.push((
        "nanosecond counter",
        (0..300)
            .map(|i: i64| (i * 41_237_119) as f64 / 1e9)
            .collect(),
    ));
    // Millicores: three decimal places.
    out.push((
        "millicores",
        (0..300).map(|i: i64| (i % 250) as f64 / 1e3).collect(),
    ));
    out.push(("negative", (0..300).map(|i: i64| -(i as f64)).collect()));
    // Negative zero is the one value that survives `==` but not the bits.
    out.push(("negative zero", vec![-0.0, 0.0, -0.0, 1.0, -0.0]));
    // Irrational values cannot be recovered as scaled integers at all.
    out.push((
        "irrational",
        (0..300)
            .map(|i| (i as f64).sqrt() * std::f64::consts::PI)
            .collect(),
    ));
    out.push((
        "not a number",
        vec![1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 2.0],
    ));
    out.push(("huge", vec![1e300, -1e300, f64::MAX, f64::MIN]));
    out.push(("single", vec![7.5]));
    out.push(("pair", vec![1.0, 2.0]));
    out
}

#[test]
fn a_chunk_returns_every_sample_bit_for_bit() {
    for (name, values) in sample_shapes() {
        let samples: Vec<crate::store::Sample> = values
            .iter()
            .enumerate()
            .map(|(i, v)| crate::store::Sample {
                t: 1_700_000_000 + i as i64 * 30,
                v: *v,
            })
            .collect();

        let chunk = crate::chunk::encode(&samples);
        let mut back = Vec::new();
        crate::chunk::decode_into(&chunk, &mut back).expect("decode");

        assert_eq!(back.len(), samples.len(), "{name}: sample count");
        for (got, want) in back.iter().zip(&samples) {
            assert_eq!(got.t, want.t, "{name}: timestamp");
            // Bit-for-bit, not approximately: a counter that loses its low
            // bits produces a rate that is quietly wrong rather than
            // visibly broken. NaN compares unequal, so compare the bits.
            assert_eq!(
                got.v.to_bits(),
                want.v.to_bits(),
                "{name}: value {} != {}",
                got.v,
                want.v
            );
        }
    }
}

#[test]
fn an_irregular_scrape_still_round_trips() {
    // Timestamps are only usually evenly spaced: a slow API server, a
    // missed scrape and a restart all break the run.
    let gaps = [30, 30, 31, 30, 90, 30, 30, 1, 3_600, 30, 29, 30];
    let mut t = 1_700_000_000;
    let mut samples = Vec::new();
    for (i, gap) in gaps.iter().cycle().take(240).enumerate() {
        t += gap;
        samples.push(crate::store::Sample {
            t,
            v: (i as f64) * 1.25,
        });
    }

    let chunk = crate::chunk::encode(&samples);
    let mut back = Vec::new();
    crate::chunk::decode_into(&chunk, &mut back).expect("decode");
    assert_eq!(back, samples);
}

#[test]
fn compression_holds_the_footprint_the_readme_claims() {
    // A day of the metrics Lens actually charts, shaped like the ones
    // measured on a real cluster: a wandering memory gauge, a nanosecond
    // CPU counter, a network counter, and a flat resource limit.
    const DAY: i64 = 86_400;
    const INTERVAL: i64 = 30;
    let mut store = Store::new(DAY);

    let pods = 96;
    for pod in 0..pods {
        let pod_name = format!("api-{pod}");
        let pairs = [
            ("namespace", "app"),
            ("pod", pod_name.as_str()),
            ("container", "api"),
            ("image", "registry.example.com/app/api:v1.2.3"),
        ];
        let mut t = 0;
        let mut n = 0i64;
        while t < DAY {
            store.append(
                labels("container_memory_working_set_bytes", &pairs),
                t,
                (268_435_456 + (n % 512) * 4_096 + pod * 1_024) as f64,
            );
            store.append(
                labels("container_cpu_usage_seconds_total", &pairs),
                t,
                (n * (37_000_000 + pod * 131)) as f64 / 1e9,
            );
            store.append(
                labels("container_network_receive_bytes_total", &pairs),
                t,
                (n * (81_923 + pod * 7)) as f64,
            );
            store.append(labels("kube_pod_container_resource_limits", &pairs), t, 2.0);
            t += INTERVAL;
            n += 1;
        }
    }

    let samples = store.sample_count();
    let bytes = store.heap_size();
    let per_sample = bytes as f64 / samples as f64;

    assert_eq!(store.series_count(), pods as usize * 4);
    // Raw would be 16 bytes a sample. The measurement on the real cluster
    // came out at 1.3; leave headroom for the head chunk, which is always
    // uncompressed, but fail if this ever drifts back toward raw.
    assert!(
        per_sample < 2.0,
        "{per_sample:.2} bytes/sample over {samples} samples ({bytes} bytes) \
         -- compression has regressed"
    );
}

#[test]
fn a_sealed_series_answers_exactly_as_an_unsealed_one_would() {
    // The store splits every series into compressed chunks and a raw head.
    // Queries must not be able to tell, wherever the boundary falls.
    const RETENTION: i64 = 86_400;
    const INTERVAL: i64 = 30;
    let mut store = Store::new(RETENTION);
    let pairs = [("namespace", "app"), ("pod", "api-1"), ("container", "api")];

    let mut reference: Vec<crate::store::Sample> = Vec::new();
    let mut t = 1_700_000_000;
    for n in 0..1_000i64 {
        // Values chosen so some chunks take the integer path and some the
        // raw one, and the boundary is not aligned to the chunk size.
        let v = if n % 300 < 150 {
            (n * 4_096) as f64
        } else {
            (n as f64).sqrt() * std::f64::consts::E
        };
        store.append(labels("container_memory_working_set_bytes", &pairs), t, v);
        reference.push(crate::store::Sample { t, v });
        t += INTERVAL;
    }

    let series = store
        .series_iter()
        .next()
        .expect("the series should be there");
    assert!(
        !series.sealed_chunks().is_empty(),
        "the test needs the series to have sealed at least one chunk"
    );
    assert_eq!(series.samples(), reference);
    assert_eq!(series.first_t(), reference.first().map(|s| s.t));
    assert_eq!(series.last_t(), reference.last().map(|s| s.t));

    let last = reference.last().unwrap().t;
    for at in [
        reference[0].t,
        reference[0].t - 1,
        reference[17].t,
        reference[239].t,
        reference[240].t,
        reference[241].t,
        reference[500].t + 7,
        last,
        last + INTERVAL * 10,
    ] {
        // value_at: the newest sample at or before `at`, within lookback.
        const LOOKBACK: i64 = 300;
        let want = reference
            .iter()
            .rev()
            .find(|s| s.t <= at)
            .filter(|s| at - s.t <= LOOKBACK)
            .map(|s| s.v);
        assert_eq!(
            series.value_at(at, LOOKBACK).map(f64::to_bits),
            want.map(f64::to_bits),
            "value_at({at})"
        );

        for window in [INTERVAL, 60, 300, 3_600, 86_400] {
            let want: Vec<_> = reference
                .iter()
                .filter(|s| s.t > at - window && s.t <= at)
                .copied()
                .collect();
            assert_eq!(
                series.range_at(at, window),
                want,
                "range_at({at}, {window})"
            );
        }
    }
}

#[test]
fn a_version_1_snapshot_still_loads() {
    // Upgrading in place must not cost a day of history, so the loader
    // still reads the raw-sample layout that shipped before chunking.
    use std::io::Write;

    let path = temp_path("v1");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");

    let original = fixture();
    let mut out = Vec::new();
    out.extend_from_slice(b"LMD1");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(original.series_count() as u32).to_le_bytes());
    for series in original.series_iter() {
        out.extend_from_slice(&(series.labels.len() as u16).to_le_bytes());
        for (key, value) in &series.labels {
            out.extend_from_slice(&(key.len() as u16).to_le_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        let samples = series.samples();
        out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        for sample in &samples {
            out.extend_from_slice(&sample.t.to_le_bytes());
            out.extend_from_slice(&sample.v.to_le_bytes());
        }
    }
    std::fs::File::create(&path)
        .expect("create")
        .write_all(&out)
        .expect("write");

    let restored = crate::snapshot::load(&path, 86_400, LAST).expect("a v1 snapshot should load");
    assert_eq!(restored.series_count(), original.series_count());
    assert_eq!(restored.sample_count(), original.sample_count());
    assert_eq!(
        one(
            &restored,
            r#"sum(kubelet_running_pods{instance=~"node-a"})"#
        ),
        one(
            &original,
            r#"sum(kubelet_running_pods{instance=~"node-a"})"#
        ),
    );

    // And the next save rewrites it in the current layout.
    crate::snapshot::save(&restored, &path).expect("save");
    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(&bytes[..4], b"LMD1", "the magic never changes");
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 2);
}

#[test]
fn a_snapshot_is_far_smaller_than_the_samples_it_holds() {
    const DAY: i64 = 86_400;
    let mut store = Store::new(DAY);
    let pairs = [("namespace", "app"), ("pod", "api-1"), ("container", "api")];
    let mut t = 0;
    let mut n = 0i64;
    while t < DAY {
        store.append(
            labels("container_cpu_usage_seconds_total", &pairs),
            t,
            (n * 37_000_000) as f64 / 1e9,
        );
        t += 30;
        n += 1;
    }

    let path = temp_path("small");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    crate::snapshot::save(&store, &path).expect("save");

    let on_disk = std::fs::metadata(&path).expect("stat").len() as usize;
    let raw = store.sample_count() * 16;
    assert!(
        on_disk * 4 < raw,
        "snapshot is {on_disk} bytes against {raw} raw -- the file should \
         hold the compressed chunks, not expanded samples"
    );

    // And it still round-trips.
    let restored = crate::snapshot::load(&path, DAY, t).expect("load");
    assert_eq!(restored.sample_count(), store.sample_count());
}

#[test]
fn a_mutated_chunk_is_rejected_rather_than_trusted() {
    // Chunk payloads are now read straight off disk and drive lengths,
    // arithmetic and allocations in the decoder. A flipped bit in one has
    // to come back as an error, never a panic and never a wrong series.
    let dir = temp_path("mutated");
    let dir = dir.parent().unwrap().to_path_buf();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A store with enough samples to have sealed several chunks.
    const RETENTION: i64 = 86_400;
    let mut store = Store::new(RETENTION);
    let pairs = [("namespace", "app"), ("pod", "api-1"), ("container", "api")];
    for n in 0..1_000i64 {
        store.append(
            labels("container_cpu_usage_seconds_total", &pairs),
            n * 30,
            (n * 37_000_000) as f64 / 1e9,
        );
    }
    let good = dir.join("good.bin");
    crate::snapshot::save(&store, &good).expect("save");
    let bytes = std::fs::read(&good).unwrap();

    // Walk a flipped bit through the whole file. Every outcome is
    // acceptable except a panic or a series that loads with the wrong
    // shape, so the assertion is about what came back, not that it failed:
    // some bytes are genuinely value payload and a flip there is a
    // different-but-valid sample.
    let mut loaded = 0;
    let mut rejected = 0;
    for offset in 0..bytes.len() {
        for bit in [0x01u8, 0x80u8] {
            let mut mutated = bytes.clone();
            mutated[offset] ^= bit;
            let path = dir.join("mutated.bin");
            std::fs::write(&path, &mutated).unwrap();

            match crate::snapshot::load(&path, RETENTION, 30_000) {
                Err(_) => rejected += 1,
                Ok(restored) => {
                    loaded += 1;
                    // Whatever survived has to be internally consistent:
                    // ascending, and readable end to end.
                    for series in restored.series_iter() {
                        let samples = series.samples();
                        assert_eq!(
                            samples.len(),
                            series.len(),
                            "length disagrees with contents"
                        );
                        assert!(
                            samples.windows(2).all(|w| w[0].t < w[1].t),
                            "a mutated snapshot produced non-ascending timestamps"
                        );
                    }
                }
            }
        }
    }

    // A header that claims far more samples than the payload can hold is
    // the shape that matters most: run-length streams expand, so nothing
    // downstream bounds it, and a load must not try to reserve for it.
    {
        let mut bomb = bytes.clone();
        // The first chunk's count field: magic(4) + version(2) + series(4)
        // + labels, then first_t(8) + last_t(8).
        let offset = bomb
            .windows(4)
            .position(|w| w == 120u32.to_le_bytes())
            .expect("a sealed chunk's sample count should be in the file");
        bomb[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let path = dir.join("bomb.bin");
        std::fs::write(&path, &bomb).unwrap();
        let error = match crate::snapshot::load(&path, RETENTION, 30_000) {
            Err(error) => error,
            Ok(_) => panic!("a chunk claiming u32::MAX samples should be rejected"),
        };
        assert!(
            format!("{error:#}").contains("implausible chunk sample count"),
            "rejected for the wrong reason: {error:#}"
        );
    }

    assert!(rejected > 0, "no mutation was rejected at all");
    assert!(
        loaded > 0,
        "every mutation was rejected, so this proves little"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
