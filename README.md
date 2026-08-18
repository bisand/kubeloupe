# lens-metricsd

Metrics for Lens Desktop on a small cluster, in one static binary.

Lens will not draw a chart without a Prometheus-compatible query API. The
usual way to get one costs a Prometheus, a node-exporter and a
kube-state-metrics — three workloads and roughly 170 MB of RAM to produce
about twenty series that the API server and the kubelet already know.

This reads them directly, keeps a day of samples in memory, and answers
the subset of PromQL that Lens actually generates.

|                         | images (compressed) | resident |
| ----------------------- | ------------------: | -------: |
| Prometheus + 2 exporters |            141.7 MB |  ~250 MB |
| VictoriaMetrics + 2 exporters |        50.6 MB |  ~170 MB |
| **lens-metricsd**        |          **2.0 MB** | **0.9 MiB** |

Measured on a single-node k3s box running ~10 pods: 0.9 MiB working set
and 0.3 millicores. That grows as the ring buffer fills — 118 series x
2880 samples x 16 bytes is about 5 MB at 24h steady state.

## How it works

Lens builds every query from one `switch` statement — `getLensLikeQueryFor`
in `packages/technical-features/prometheus/src/lens-provider.injectable.ts`
— interpolating only node, pod and namespace names. The grammar is
therefore finite, and small: selectors with `=`/`!=`/`=~`, `rate(x[1m])`,
`sum() by ()`, and `+`/`-` between vectors. `src/promql` implements
exactly that, structurally rather than by matching the ~40 query strings,
so a Lens update that reorders a label does not blank the graphs.

The data comes from two places, both on the API server:

| Lens asks for                    | comes from                          |
| -------------------------------- | ----------------------------------- |
| `container_*`                    | kubelet `/stats/summary` via nodes/proxy |
| `node_*`                         | the same, node section              |
| `kubelet_running_pods`, `kubelet_volume_stats_*` | the same          |
| `kube_node_status_*`             | Node objects                        |
| `kube_pod_container_resource_*`  | Pod objects                         |

Nothing here reimplements node-exporter. The values are chosen so that
the *arithmetic Lens performs* comes out right — the clearest case being
memory, where Lens computes `MemTotal - (MemFree + Buffers + Cached)`, so
publishing capacity, kubelet's `availableBytes` and two zeroes yields the
working set without ever reading `/proc/meminfo` or mounting a hostPath.

Two label conventions are load-bearing and easy to get wrong: kubelet and
cadvisor series must carry `instance` = node **name** (not `ip:port`),
because Lens interpolates node names; and node series must carry
`kubernetes_node`, because Lens groups by it.

## What it does not cover

- **Per-container disk read/write rates.** `container_fs_reads_bytes_total`
  and `container_fs_writes_bytes_total` exist only in cadvisor's
  `/metrics/cadvisor`, not in `/stats/summary`. The pod Filesystem chart
  shows usage; its read and write lines stay empty.
- **Ingress metrics**, which Lens expects from an nginx ingress
  controller's own exporter.
- **Per-PVC usage on hostPath-backed storage.** With k3s local-path, the
  kubelet reports every volume's `usedBytes` as the usage of the
  *underlying filesystem*, so two claims on one node show the same
  figure. That is the kubelet's own output, not a mapping error here —
  a Prometheus scraping `kubelet_volume_stats_*` reports it identically.
  Claims on a storage class with real quotas report real per-volume
  usage.

## Build

    docker build --platform linux/amd64 -t lens-metricsd:0.1.0 .

The builder is deliberately not pinned to `$BUILDPLATFORM`: it must run as
the target platform so the binary matches the node. Building on an arm64
laptop for an amd64 node goes through emulation and takes a few minutes.

## Deploy

    docker tag lens-metricsd:0.1.0 ghcr.io/bisand/lens-metricsd:0.1.0
    docker push ghcr.io/bisand/lens-metricsd:0.1.0
    kubectl apply -f deploy/lens-metricsd.yaml

The manifest uses `imagePullPolicy: IfNotPresent`, so a node that already
holds the tag never depends on the registry being reachable.

**The ghcr package must be public**, or the kubelet needs an
`imagePullSecret` — packages default to private on first push. No registry
is needed at all if you would rather import straight into containerd:

    docker save lens-metricsd:0.1.0 | ssh bisand@postbud 'sudo k3s ctr images import -'

## Verify

    kubectl -n lens-metrics get pods
    kubectl get --raw "/api/v1/namespaces/lens-metrics/services/prometheus:80/proxy/-/stats"
    kubectl get --raw "/api/v1/namespaces/lens-metrics/services/prometheus:80/proxy/api/v1/query?query=up"

Charts need about two minutes of samples before a `rate()` has anything
to draw, and the store is in memory — a pod restart resets history.

### Two Lens-side settings that will otherwise look like a server fault

Both live in Cluster Settings → Metrics, and both fail *silently*:

1. **METRICS SOURCE must be set to Prometheus explicitly.** Left on
   `Automatic`, Lens Desktop resolves to "Kubernetes Metrics Server" and
   reports *"Metrics are not available due to missing or invalid
   configuration"* — even with this daemon running and answering. It does
   not fall through to a Prometheus-compatible service on its own.
2. **HIDE METRICS FROM THE UI must be empty.** If every resource type is
   listed there, the cluster overview renders only the issues panel — no
   time-range dropdown, no donuts, no charts, and no error either. The
   `Reset` button clears it.

Leave the **Lens Metrics** page's three toggles (bundled Prometheus,
kube-state-metrics, node-exporter) OFF. Those install the stack this
daemon exists to replace.

## Configuration

| variable                 | default        |                                            |
| ------------------------ | -------------- | ------------------------------------------ |
| `SCRAPE_INTERVAL_SECONDS` | `30`          | Halving it doubles the store's memory.      |
| `RETENTION_HOURS`         | `24`          | Covers Lens' longest range.                 |
| `LISTEN_ADDR`             | `0.0.0.0:9090` |                                            |

## Tests

    cargo test

The suite evaluates the real Lens query strings against a synthetic
cluster and asserts the numbers — including the two failure modes that
would be invisible in a chart: an unanchored pod regex silently summing
`api-1` with `api-10`, and a container restart reading as a negative rate.
