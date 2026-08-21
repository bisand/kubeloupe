//! Turning API objects and kubelet stats into the series Lens asks for.
//!
//! The metric names below are node-exporter's, cadvisor's and
//! kube-state-metrics'. Nothing here is pretending to be a general
//! implementation of those exporters -- each series exists because a Lens
//! query names it, and the values are chosen so that the ARITHMETIC LENS
//! PERFORMS comes out right. The clearest case is memory: Lens computes
//!
//!     MemTotal - (MemFree + Buffers + Cached)
//!
//! so publishing capacity, kubelet's `availableBytes`, and two zeroes
//! makes that expression evaluate to the working set -- the number the
//! chart is supposed to show -- without this daemon ever reading
//! /proc/meminfo or needing a hostPath mount to do it.

use crate::kube::{Client, List, Pod, parse_quantity};
use crate::store::{Store, labels};
use std::collections::HashMap;

/// Non-empty on purpose. Lens filters container series with `image!=""`,
/// so a container whose image we cannot resolve must still carry one.
const UNKNOWN_IMAGE: &str = "unknown";

/// The pod list, held between scrapes.
///
/// It is the largest thing this daemon reads -- on a 107-pod cluster it is
/// 0.9 MB of JSON against 0.5 MB for every kubelet summary put together --
/// and almost none of it changes between scrapes. Container images and
/// resource requests are fixed for the life of a pod; only the set of pods
/// moves, and it moves on the timescale of a rollout, not a scrape.
///
/// So it is refetched on its own slower cadence and the series derived
/// from it are still appended every scrape, from the held copy.
///
/// Left at that, the interval would widen a real hazard. A container the
/// list has not seen gets `image="unknown"`, and when the real image
/// arrives it starts a *different* series -- so for one `LOOKBACK` both
/// answer, and `sum(...) by (pod)` counts the pod twice. A 30s window for
/// that is a sample; a 5m window is ten. So a scrape that meets a
/// container it cannot name marks the list stale and the next one
/// refetches, which bounds the window to one scrape either way and leaves
/// the interval to govern only the case where nothing changed.
///
/// What the interval does still cost: a pod deleted just after a refresh
/// keeps receiving `kube_pod_container_resource_*` samples until the next
/// one. Those are the requests and limits it was configured with, they do
/// not move, and retention drops them like any other stale series.
pub struct PodCache {
    pods: List<Pod>,
    fetched_at: i64,
    /// Set when a scrape saw a container this list cannot name, so the
    /// next one refetches regardless of the interval.
    stale: bool,
}

impl PodCache {
    #[cfg(test)]
    pub fn for_test(fetched_at: i64) -> Option<Self> {
        Some(Self {
            pods: List { items: Vec::new() },
            fetched_at,
            stale: false,
        })
    }

    #[cfg(test)]
    pub fn mark_stale_for_test(&mut self) {
        self.stale = true;
    }

    pub(crate) fn needs_refresh(cache: &Option<Self>, now: i64, every: i64) -> bool {
        match cache {
            None => true,
            Some(held) => held.stale || now - held.fetched_at >= every,
        }
    }
}

pub async fn collect(
    client: &Client,
    store: &mut Store,
    now: i64,
    cache: &mut Option<PodCache>,
    refresh_every: i64,
) -> anyhow::Result<()> {
    let nodes = client.nodes().await?;

    if PodCache::needs_refresh(cache, now, refresh_every) {
        // A refresh that fails is not fatal while a previous list is held:
        // the series it feeds are better a few minutes stale than absent,
        // and the next scrape tries again. With nothing held there is
        // nothing to fall back on, so the error propagates as before.
        match client.pods().await {
            Ok(pods) => {
                *cache = Some(PodCache {
                    pods,
                    fetched_at: now,
                    stale: false,
                });
            }
            Err(error) => match cache {
                Some(held) => eprintln!(
                    "kubeloupe: refreshing the pod list failed, reusing the one from {}s ago: {error:#}",
                    now - held.fetched_at
                ),
                None => return Err(error),
            },
        }
    }

    let pods = &cache
        .as_ref()
        .expect("a pod list is held once the first fetch has succeeded")
        .pods;

    // (namespace, pod, container) -> image, so container series can carry
    // the label cadvisor would have provided.
    let mut images: HashMap<(String, String, String), String> = HashMap::new();

    for pod in &pods.items {
        let ns = &pod.metadata.namespace;
        let name = &pod.metadata.name;

        for container in &pod.spec.containers {
            images.insert(
                (ns.clone(), name.clone(), container.name.clone()),
                container.image.clone(),
            );
        }
        // The status image is the resolved one (digest, defaulted
        // registry); prefer it where the kubelet has reported it.
        for status in &pod.status.container_statuses {
            if !status.image.is_empty() {
                images.insert(
                    (ns.clone(), name.clone(), status.name.clone()),
                    status.image.clone(),
                );
            }
        }

        for container in &pod.spec.containers {
            for (metric, quantities) in [
                (
                    "kube_pod_container_resource_requests",
                    &container.resources.requests,
                ),
                (
                    "kube_pod_container_resource_limits",
                    &container.resources.limits,
                ),
            ] {
                for (resource, quantity) in quantities {
                    let Some(value) = parse_quantity(quantity) else {
                        continue;
                    };
                    let unit = unit_for(resource);
                    store.append(
                        labels(
                            metric,
                            &[
                                ("namespace", ns),
                                ("pod", name),
                                ("container", &container.name),
                                ("node", &pod.spec.node_name),
                                ("resource", resource),
                                ("unit", unit),
                            ],
                        ),
                        now,
                        value,
                    );
                }
            }
        }
    }

    // A container the kubelet reports that the held pod list has never
    // seen means the list is stale in the way that matters. Counting them
    // is what keeps the refresh interval from widening the window in which
    // a new pod's series carry `image="unknown"` -- see [`PodCache`].
    let mut unnamed = 0usize;

    for node in &nodes.items {
        let name = &node.metadata.name;

        for (metric, quantities) in [
            ("kube_node_status_capacity", &node.status.capacity),
            ("kube_node_status_allocatable", &node.status.allocatable),
        ] {
            for (resource, quantity) in quantities {
                let Some(value) = parse_quantity(quantity) else {
                    continue;
                };
                store.append(
                    labels(
                        metric,
                        &[
                            ("node", name),
                            ("resource", resource),
                            ("unit", unit_for(resource)),
                        ],
                    ),
                    now,
                    value,
                );
            }
        }

        // One node failing to answer must not cost the others their
        // sample, so this is reported and stepped over rather than
        // returned.
        match client.node_stats(name).await {
            Ok(stats) => {
                unnamed += collect_node_stats(store, name, &stats, &images, now);
                store.append(
                    labels("up", &[("instance", name), ("job", "kubeloupe")]),
                    now,
                    1.0,
                );
            }
            Err(error) => {
                eprintln!("kubeloupe: stats/summary for node {name} failed: {error:#}");
                store.append(
                    labels("up", &[("instance", name), ("job", "kubeloupe")]),
                    now,
                    0.0,
                );
            }
        }
    }

    if unnamed > 0
        && let Some(held) = cache.as_mut()
    {
        held.stale = true;
    }

    Ok(())
}

/// Returns how many containers the kubelet reported that the held pod list
/// does not know an image for -- see [`PodCache`].
fn collect_node_stats(
    store: &mut Store,
    node: &str,
    stats: &crate::kube::StatsSummary,
    images: &HashMap<(String, String, String), String>,
    now: i64,
) -> usize {
    // Three names for one node, because the query grammar decides which
    // one is read. Lens filters kubelet and cadvisor series on `instance`
    // and groups node series by `kubernetes_node`; the Helm grammar uses
    // `node` for both. Carrying all three is what lets the same series
    // answer either provider -- and `node` is the one that makes the
    // daemon addressable, since Helm is configurable and Lens is not.
    let node_labels: [(&str, &str); 3] = [
        ("kubernetes_node", node),
        ("instance", node),
        ("node", node),
    ];

    if let Some(memory) = &stats.node.memory {
        let available = memory.available_bytes.unwrap_or(0.0);
        let working_set = memory
            .working_set_bytes
            .or(memory.usage_bytes)
            .unwrap_or(0.0);

        for (metric, value) in [
            ("node_memory_MemTotal_bytes", available + working_set),
            ("node_memory_MemFree_bytes", available),
            // Kubelet already accounts for page cache in the working set,
            // so these are zero rather than unreported: Lens subtracts
            // them, and an absent series would make the whole expression
            // drop out by one-to-one matching.
            ("node_memory_Buffers_bytes", 0.0),
            ("node_memory_Cached_bytes", 0.0),
        ] {
            store.append(labels(metric, &node_labels), now, value);
        }
    }

    if let Some(cpu) = &stats.node.cpu
        && let Some(nanos) = cpu.usage_core_nano_seconds
    {
        // A cumulative counter, which is what Lens' rate() needs. The
        // single `mode="user"` series is enough: Lens sums over
        // `mode=~"user|system"` and wants total cores either way.
        store.append(
            labels(
                "node_cpu_seconds_total",
                &[
                    ("kubernetes_node", node),
                    ("instance", node),
                    ("node", node),
                    ("mode", "user"),
                    ("cpu", "0"),
                ],
            ),
            now,
            nanos / 1e9,
        );
    }

    if let Some(fs) = &stats.node.fs {
        // Lens' default mountpoint filter is `/|/local`; the kubelet
        // reports one root filesystem, which is the `/` of that pair.
        let fs_labels: [(&str, &str); 4] = [
            ("kubernetes_node", node),
            ("instance", node),
            ("node", node),
            ("mountpoint", "/"),
        ];
        if let Some(capacity) = fs.capacity_bytes {
            store.append(
                labels("node_filesystem_size_bytes", &fs_labels),
                now,
                capacity,
            );
        }
        if let Some(available) = fs.available_bytes {
            store.append(
                labels("node_filesystem_avail_bytes", &fs_labels),
                now,
                available,
            );
        }
    }

    store.append(
        labels("kubelet_running_pods", &[("instance", node)]),
        now,
        stats.pods.len() as f64,
    );

    let mut unnamed = 0usize;
    for pod in &stats.pods {
        let ns = &pod.pod_ref.namespace;
        let name = &pod.pod_ref.name;

        for container in &pod.containers {
            let known = images
                .get(&(ns.clone(), name.clone(), container.name.clone()))
                .map(String::as_str)
                .filter(|image| !image.is_empty());
            if known.is_none() {
                unnamed += 1;
            }
            let image = known.unwrap_or(UNKNOWN_IMAGE);

            let container_labels: [(&str, &str); 5] = [
                ("namespace", ns),
                ("pod", name),
                ("container", &container.name),
                ("image", image),
                ("instance", node),
            ];

            if let Some(cpu) = &container.cpu
                && let Some(nanos) = cpu.usage_core_nano_seconds
            {
                store.append(
                    labels("container_cpu_usage_seconds_total", &container_labels),
                    now,
                    nanos / 1e9,
                );
            }

            if let Some(memory) = &container.memory
                && let Some(working_set) = memory.working_set_bytes.or(memory.usage_bytes)
            {
                store.append(
                    labels("container_memory_working_set_bytes", &container_labels),
                    now,
                    working_set,
                );
            }

            // Writable layer plus logs, which is what a container is
            // actually costing the node's disk.
            let rootfs = container.rootfs.as_ref().and_then(|fs| fs.used_bytes);
            let logs = container.logs.as_ref().and_then(|fs| fs.used_bytes);
            if rootfs.is_some() || logs.is_some() {
                store.append(
                    labels("container_fs_usage_bytes", &container_labels),
                    now,
                    rootfs.unwrap_or(0.0) + logs.unwrap_or(0.0),
                );
            }
        }

        // Network is per-pod in the kubelet's accounting -- every
        // container shares the sandbox's interface -- so these carry no
        // container label. Lens' network queries do not filter on one.
        if let Some(network) = &pod.network {
            let network_labels: [(&str, &str); 3] =
                [("namespace", ns), ("pod", name), ("instance", node)];
            if let Some(rx) = network.rx_bytes {
                store.append(
                    labels("container_network_receive_bytes_total", &network_labels),
                    now,
                    rx,
                );
            }
            if let Some(tx) = network.tx_bytes {
                store.append(
                    labels("container_network_transmit_bytes_total", &network_labels),
                    now,
                    tx,
                );
            }
        }

        for volume in &pod.volume {
            let Some(pvc) = &volume.pvc_ref else { continue };
            let pvc_labels: [(&str, &str); 2] = [
                ("persistentvolumeclaim", &pvc.name),
                ("namespace", &pvc.namespace),
            ];
            if let Some(used) = volume.used_bytes {
                store.append(
                    labels("kubelet_volume_stats_used_bytes", &pvc_labels),
                    now,
                    used,
                );
            }
            if let Some(capacity) = volume.capacity_bytes {
                store.append(
                    labels("kubelet_volume_stats_capacity_bytes", &pvc_labels),
                    now,
                    capacity,
                );
            }
        }
    }

    unnamed
}

fn unit_for(resource: &str) -> &'static str {
    match resource {
        "cpu" => "core",
        "memory" | "ephemeral-storage" | "storage" => "byte",
        _ => "integer",
    }
}
