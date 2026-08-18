# lens-metricsd

**Metrics for [Lens Desktop](https://k8slens.dev) in one static binary.**
No Prometheus, no node-exporter, no kube-state-metrics.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Image size](https://img.shields.io/badge/image-2.0%20MB-green)
![Resident](https://img.shields.io/badge/resident-~1%20MiB-green)

Lens will not draw a chart without a Prometheus-compatible query API. The
usual way to get one costs three workloads and roughly 170 MB of RAM — to
produce about twenty series that the Kubernetes API server and the kubelet
already know.

This reads them directly, keeps a day of samples in memory, and answers
the subset of PromQL that Lens actually generates.

| | image (compressed) | resident |
| --- | ---: | ---: |
| Prometheus + node-exporter + kube-state-metrics | 141.7 MB | ~250 MB |
| VictoriaMetrics + node-exporter + kube-state-metrics | 50.6 MB | ~170 MB |
| **lens-metricsd** | **2.0 MB** | **~1 MiB** |

Measured on a single-node k3s box (1 core, 2 GB) running ~10 pods: 0.9 MiB
working set, 0.3 millicores. It grows as the ring buffer fills — about
5 MB at 24h steady state for ~120 series.

Built for small clusters — homelabs, single-node k3s, edge boxes — where a
monitoring stack costs more than the workloads it watches.

## Quick start

```sh
kubectl apply -f https://raw.githubusercontent.com/bisand/lens-metricsd/main/deploy/lens-metricsd.yaml
```

That creates a `lens-metrics` namespace, a read-only ServiceAccount, one
Deployment, and a Service named `prometheus`.

**The Service name and namespace are load-bearing.** Lens' built-in
provider does a literal lookup of the Service `prometheus` in namespace
`lens-metrics` and takes `ports[0].port`. Rename either and Lens stops
finding it.

Then configure Lens — see below. Charts need about two minutes of samples
before a `rate()` has anything to draw.

## Configuring Lens

In **Cluster Settings → Metrics**, two settings will otherwise make a
working daemon look broken. Both fail *silently*:

1. **Set METRICS SOURCE to Prometheus.** Left on `Automatic`, Lens
   Desktop may resolve to "Kubernetes Metrics Server" and report
   *"Metrics are not available due to missing or invalid configuration"* —
   even with this daemon running and answering correctly. It does not fall
   through to a Prometheus-compatible service on its own.
2. **Make sure HIDE METRICS FROM THE UI is empty.** If resource types are
   listed there, the cluster overview renders only the issues panel — no
   time-range dropdown, no donuts, no charts, and no error explaining why.
   The `Reset` button clears it.

Leave the **Lens Metrics** page's three toggles (bundled Prometheus,
kube-state-metrics, node-exporter) **off**. Those install the stack this
daemon exists to replace.

## Verify

```sh
kubectl -n lens-metrics get pods
kubectl get --raw "/api/v1/namespaces/lens-metrics/services/prometheus:80/proxy/-/stats"
kubectl get --raw "/api/v1/namespaces/lens-metrics/services/prometheus:80/proxy/api/v1/query?query=up"
```

`/-/stats` reports series and sample counts; `up` is 1 per node whose
kubelet answered.

## How it works

Lens builds every query from one `switch` statement —
`getLensLikeQueryFor` in `lens-provider.injectable.ts` — interpolating
only node, pod and namespace names. The grammar is therefore finite and
small: selectors with `=`/`!=`/`=~`/`!~`, `rate(x[1m])`, `sum() by ()`,
and arithmetic between vectors. [`src/promql`](src/promql) implements
exactly that — structurally, rather than by matching the ~40 query
strings, so a Lens update that reorders a label does not blank the graphs.

Everything comes from the API server, over one connection:

| Lens asks for | comes from |
| --- | --- |
| `container_*` | kubelet `/stats/summary`, via `nodes/proxy` |
| `node_*` | the same, node section |
| `kubelet_running_pods`, `kubelet_volume_stats_*` | the same |
| `kube_node_status_*` | Node objects |
| `kube_pod_container_resource_*` | Pod objects |

Nothing here reimplements node-exporter. The values are chosen so that the
*arithmetic Lens performs* comes out right — the clearest case being
memory, where Lens computes `MemTotal - (MemFree + Buffers + Cached)`, so
publishing capacity, the kubelet's `availableBytes` and two zeroes yields
the working set. No `/proc/meminfo`, no hostPath mount, no DaemonSet.

Going through the API server's node proxy rather than straight to port
10250 means one host, one CA and one token — and no `insecure_skip_verify`
anywhere.

Two label conventions are load-bearing and easy to get wrong: kubelet and
cadvisor series must carry `instance` = node **name** (not `ip:port`),
because Lens interpolates node names; and node series must carry
`kubernetes_node`, because Lens groups by it. Get these wrong and you have
a working scrape with blank charts.

## What it does not cover

- **Per-container disk read/write rates.** `container_fs_reads_bytes_total`
  and `container_fs_writes_bytes_total` exist only in cadvisor's
  `/metrics/cadvisor`, not in `/stats/summary`. The pod Filesystem chart
  shows usage; its read and write lines stay empty.
- **Ingress metrics**, which Lens expects from an nginx ingress
  controller's own exporter.
- **Per-PVC usage on hostPath-backed storage.** With k3s local-path, the
  kubelet reports every volume's `usedBytes` as the usage of the
  *underlying filesystem*, so two claims on one node show the same figure.
  That is the kubelet's own output, not a mapping error here — a
  Prometheus scraping `kubelet_volume_stats_*` reports it identically.
  Storage classes with real quotas report real per-volume usage.
- **High availability.** The series live in the process, so run exactly
  one replica: a second would hold a different history and the Service
  would alternate between them.

## Persistence

The store is in memory, but it is snapshotted to disk so a restart does
not cost a day of history. A **SIGTERM — every ordinary rollout — writes a
final snapshot and loses nothing**; an unclean kill costs at most
`SNAPSHOT_INTERVAL_SECONDS`.

It is a periodic snapshot rather than a write-through store on purpose:
Lens re-queries every chart once a minute over ranges up to 24h, so
reading from disk would mean constant I/O to serve a working set that
fits in 9 MB of RAM. One sequential write every few minutes keeps the
query path a pointer walk.

Three properties matter more than the format:

- **The write is atomic** — a temporary file renamed over the target, then
  fsynced. A process killed mid-write leaves the previous good snapshot
  intact; without it, one OOM would turn into permanent data loss.
- **A bad snapshot is never fatal.** It is logged and the daemon starts
  empty. Persistence that can wedge the process into `CrashLoopBackOff` is
  worse than none.
- **Loading re-applies retention**, so a pod that was down for six hours
  does not come back with six hours of stale points.

Set `SNAPSHOT_PATH` to enable it and mount a volume there. The supplied
manifest includes a 64Mi PVC. Note the pod needs `fsGroup` matching
`runAsUser`, or the provisioner's root-owned directory is not writable and
every snapshot fails with `EACCES` — while the daemon otherwise runs
perfectly, so it surfaces only as history that never survives.
`readOnlyRootFilesystem: true` still applies: it covers the *root*
filesystem, and mounted volumes stay writable.

## Configuration

| variable | default | |
| --- | --- | --- |
| `SCRAPE_INTERVAL_SECONDS` | `30` | Halving it doubles the store's memory. |
| `RETENTION_HOURS` | `24` | Covers Lens' longest range. |
| `SNAPSHOT_PATH` | *(unset)* | Unset disables persistence, so the daemon runs fine with no volume attached. |
| `SNAPSHOT_INTERVAL_SECONDS` | `300` | Upper bound on history lost to an *unclean* kill. |
| `LISTEN_ADDR` | `0.0.0.0:9090` | |

RBAC is read-only: `get`/`list` on nodes and pods, `get` on `nodes/proxy`.

## Build

```sh
docker build --platform linux/amd64 -t ghcr.io/bisand/lens-metricsd:0.1.1 .
```

The builder is deliberately **not** pinned to `$BUILDPLATFORM`: it must run
as the target platform so the binary matches the node. Building on an
arm64 laptop for an amd64 node goes through emulation and takes a few
minutes — which is the correct trade against shipping a binary the node
cannot execute.

`rust:alpine` targets musl natively, so the binary is static and the
runtime image is `FROM scratch`: no base OS, nothing to patch.

If you would rather not use a registry at all, import straight into the
node's containerd and set `imagePullPolicy: Never`:

```sh
docker save lens-metricsd:0.1.1 | ssh <node> 'sudo k3s ctr images import -'
```

## Tests

```sh
cargo test
```

The suite evaluates the real Lens query strings against a synthetic
cluster and asserts the numbers — including failure modes that would be
invisible in a chart: an unanchored pod regex silently summing `api-1`
with `api-10`, a container restart reading as a negative rate, and a
stale series flatlining instead of breaking the line.

## License

Apache-2.0. See [LICENSE](LICENSE).
