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
    let node_labels = [("kubernetes_node", NODE), ("instance", NODE)];

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
    let container =
        [("namespace", "app"), ("pod", "api-1"), ("container", "api"), ("image", "x")];

    // Ten retention windows of continuous collection.
    let mut peak = 0;
    let mut t = 0;
    while t < RETENTION * 10 {
        store.append(labels("container_memory_working_set_bytes", &container), t, 1.0);
        store.append(labels("kubelet_running_pods", &[("instance", NODE)]), t, 5.0);
        store.prune(t);
        peak = peak.max(store.sample_count());
        t += INTERVAL;
    }

    // Two series, each holding at most one retention window of samples.
    let ceiling = 2 * (RETENTION / INTERVAL + 1) as usize;
    assert_eq!(store.series_count(), 2);
    assert!(
        peak <= ceiling,
        "peak {peak} samples exceeded the retention ceiling of {ceiling}"
    );
    // And it really did fill up rather than staying trivially small.
    assert!(peak > ceiling / 2, "expected the window to fill, peaked at {peak}");
}

#[test]
fn series_that_stop_reporting_are_dropped_entirely() {
    const RETENTION: i64 = 600;
    let mut store = Store::new(RETENTION);

    // A pod that exists briefly, as happens on every rollout.
    for t in (0..300).step_by(30) {
        store.append(
            labels("container_memory_working_set_bytes", &[
                ("namespace", "app"),
                ("pod", "doomed-1"),
                ("container", "api"),
                ("image", "x"),
            ]),
            t,
            1.0,
        );
    }
    assert_eq!(store.series_count(), 1);

    // Long after its last sample, the series -- and its name index entry --
    // must be gone, not just empty. Churning pods would otherwise
    // accumulate for the life of the process.
    store.prune(1_000);
    assert_eq!(store.series_count(), 0, "an aged-out series should be removed");
    assert_eq!(store.sample_count(), 0);

    // The name index must have been cleaned too: re-appending has to
    // produce exactly one series, not attach to a stale key.
    store.append(labels("kubelet_running_pods", &[("instance", NODE)]), 1_030, 1.0);
    assert_eq!(store.series_count(), 1);
}
