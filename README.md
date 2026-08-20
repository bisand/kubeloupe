# kubeloupe

**Metrics for [Lens Desktop](https://k8slens.dev) in one static binary.**
No Prometheus, no node-exporter, no kube-state-metrics.

[![ci](https://github.com/bisand/kubeloupe/actions/workflows/ci.yml/badge.svg)](https://github.com/bisand/kubeloupe/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/bisand/kubeloupe?sort=semver)](https://github.com/bisand/kubeloupe/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Image size](https://img.shields.io/badge/image-2%20MB-green)

Lens will not draw a chart without a Prometheus-compatible query API. The
usual way to get one costs three workloads and roughly 170 MB of RAM — to
produce about twenty series that the Kubernetes API server and the kubelet
already know.

This reads them directly, keeps a day of samples in memory, snapshots them
so a restart costs nothing, and answers the subset of PromQL that Lens
actually generates.

| | image (compressed) | resident |
| --- | ---: | ---: |
| Prometheus + node-exporter + kube-state-metrics | 141.7 MB | ~250 MB |
| VictoriaMetrics + node-exporter + kube-state-metrics | 50.6 MB | ~170 MB |
| **kubeloupe** | **2.0 MB** | **~5 MiB** |

Samples are the entire footprint, so they are compressed. Timestamps and
values become varint delta streams, and the values are first recovered as
the integers they always were — the bytes, nanoseconds and millicores that
got divided into `f64` on the way in. Replaying a day of a real 673-series
cluster through the encoder gives **1.9 bytes per sample against 16 raw,
8.3x smaller**; a test asserts it, so it cannot quietly drift back.

Only the newest hour of each series is kept raw, so appends stay a pointer
bump and instant queries never decompress anything. A 5m `rate()` over a
day of history costs about 90 µs, and the worst query Lens can send — a
24h rate across 96 pods — about 1 ms.

Memory follows the sample count rather than the cluster:
`series x (retention / scrape interval) x ~1.9 bytes`, plus labels. For
the 673-series cluster above that is 3.6 MiB of samples where the raw form
held 29.6 MiB. The shape is a plateau, not a leak: retention bounds it,
and there are tests that say so.

**The resident figure in the table is not settled yet.** The raw-sample
build — image `0.2.8`, whose daemon `0.2.9` did not change — measured
**13.3 MiB after a day** on a single-node k3s box (1 core, 2 GB, 11 pods,
370 series) and **29.3 MiB** on a three-node cluster (67 pods, 673
series), at 0.15 and 0.47 millicores. Rolled onto the same k3s box with
its day of history restored from the snapshot, this version came back at
**2.4 MiB** holding the same 346k samples. That reading is minutes old
rather than a day old, so the table rounds up and this paragraph will be
replaced once it has run a full day. The last time this README projected
instead of measuring, it was wrong by a factor of three.

Built for small clusters — homelabs, single-node k3s, edge boxes — where a
monitoring stack costs more than the workloads it watches.

## Quick start

```sh
kubectl apply -f https://raw.githubusercontent.com/bisand/kubeloupe/main/deploy/kubeloupe.yaml
```

That creates a `lens-metrics` namespace, a read-only ServiceAccount, and a
Deployment, ServiceAccount and PVC named `kubeloupe`, behind a Service
named `prometheus`.

**Those two names are chosen so Lens finds this with no configuration.**
`lens-metrics` is where Lens installs its own bundled Prometheus, and its
built-in provider does a literal lookup of the Service `prometheus` there —
that provider is declared `isConfigurable: false`, so it is the one path
into Lens that needs no address typed anywhere. Taking the address is
deliberate: it is also the reason to leave Lens' bundled metrics off, since
enabling them would put a second Service named `prometheus` in the same
namespace. If you would rather have this in its own namespace, see
[below](#if-you-would-rather-have-your-own-namespace).

Then configure Lens — see below. Charts need about two minutes of samples
before a `rate()` has anything to draw.

## Configuring Lens

In **Cluster Settings → Metrics**, two settings will otherwise make a
working daemon look broken. Both fail *silently*:

1. **Check METRICS SOURCE.** `Automatic` often resolves to Prometheus by
   itself once the Service exists: the Metrics page then reads *Currently
   used metrics source: Prometheus*, with `Provider: Lens` and the detected
   path under AUTO DETECTED PROMETHEUS DETAILS. When it does, there is
   nothing to change. But it can settle on "Kubernetes Metrics Server"
   instead and report *"Metrics are not available due to missing or invalid
   configuration"* while this daemon is running and answering correctly —
   most likely when the cluster was already open in Lens before the daemon
   was installed. Setting the source to `Prometheus` explicitly is the
   reliable fix, and costs nothing when detection would have worked.
2. **Make sure HIDE METRICS FROM THE UI is empty.** If resource types are
   listed there, the cluster overview renders only the issues panel — no
   time-range dropdown, no donuts, no charts, and no error explaining why.
   The `Reset` button clears it.

Leave **PROMETHEUS** on `Auto Detect Prometheus` with the address field
empty; detection finds the Service by name.

Leave the **Lens Metrics** page's three toggles (bundled Prometheus,
kube-state-metrics, node-exporter) **off**. Those install the stack this
daemon exists to replace.

### If you would rather have your own namespace

Nothing in the daemon depends on either name — they are chosen so Lens'
detection works. To run it in a `kubeloupe` namespace instead, there is a
kustomize overlay in [`deploy/own-namespace`](deploy/own-namespace):

```sh
kubectl apply -k https://github.com/bisand/kubeloupe//deploy/own-namespace?ref=main
```

It is twelve lines, and it builds on the same manifest rather than copying
it:

```yaml
resources:
  - https://raw.githubusercontent.com/bisand/kubeloupe/main/deploy/kubeloupe.yaml

namespace: kubeloupe

patches:
  - target:
      kind: Service
      name: prometheus
    patch: |
      - op: replace
        path: /metadata/name
        value: kubeloupe
```

`namespace:` moves every namespaced resource, renames the `Namespace`
object itself, and — the part worth having a tool do — rewrites the
`ClusterRoleBinding`'s subject to match. A subject left pointing at the old
namespace is a silent 403: the daemon runs, scrapes nothing, and says so
only in its log. Swap `main` for a tag such as `v0.2.7` in both places to
pin the version.

Lens then needs a third setting, because auto-detection will no longer find
it: set **PROMETHEUS** to `Helm` and **PROMETHEUS SERVICE ADDRESS** to
`kubeloupe/kubeloupe:80`. `Helm` is the only *configurable* provider whose
query grammar this daemon answers — its queries differ from Lens' in eight
of forty, every one a label rename (`kubernetes_node` → `node`,
`by (kubernetes_name)` → `by (component)`) that the node series carry both
spellings of.

> [!WARNING]
> Do not pick `Operator` or `Stacklight` to reveal that address field.
> Those grammars join through `kube_pod_info` with `group_left`, which this
> daemon does not implement — Lens will connect successfully and draw
> nothing, which looks like a bug here rather than a misconfiguration.

Worth it if you intend to run Lens' bundled metrics alongside this, or if
you simply want the namespace to carry the project's name. Otherwise the
default costs two settings instead of three and has no way to be
wrong-but-connected.

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

### Reading it from something other than Lens

Lens needs only `/api/v1/query` and `/api/v1/query_range`: it interpolates
node, pod and namespace names it already holds from the Kubernetes API, so
it never has to ask what exists. Anything else does, so the read-only
discovery endpoints are answered too — `/api/v1/labels`,
`/api/v1/series` and `/api/v1/label/{name}/values`, plus an empty
`/api/v1/metadata`. Grafana's metric browser is one call to
`label/__name__/values`, and a `label_values()` variable is one call to
`label/{name}/values` and nothing more. They add no collection and no
state — they read label sets the store already holds — for about 25 KiB
compressed.

What that does **not** buy is the rest of Prometheus. The PromQL subset is
still the one Lens generates: selectors, `rate()`, `sum() by ()` and
arithmetic. `avg`, `count`, `histogram_quantile`, `topk`, `offset` and
subqueries are a 422, so a prebuilt Kubernetes dashboard will mostly not
render — it expects both the functions and the full
kube-state-metrics/node-exporter catalogue. Hand-written panels within the
subset work.

`/api/v1/status/buildinfo` is deliberately **not** answered. Grafana probes
it to decide which PromQL features to send, and a 404 makes it assume the
conservative subset — which is the truth here. Answering it with a version
number would advertise functions the parser rejects.

Some values are also chosen so that the *arithmetic Lens performs* comes
out right, which is not the same as each series being independently
truthful: `node_memory_Buffers_bytes` and `node_memory_Cached_bytes` are
published as zero, and `MemTotal` as capacity, so that Lens' `MemTotal -
(MemFree + Buffers + Cached)` yields the working set. A tool that computes
memory a different way gets a wrong number, not a missing one.

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

## Upgrading

The namespace and Service are `lens-metrics` and `prometheus` again, as
they were before `0.2.4`. Coming from any earlier version, apply the
manifest and delete what the old names left behind:

```sh
kubectl apply -f https://raw.githubusercontent.com/bisand/kubeloupe/main/deploy/kubeloupe.yaml
kubectl -n lens-metrics delete deploy,sa,pvc lens-metricsd
kubectl delete clusterrole,clusterrolebinding lens-metricsd
kubectl delete namespace kubeloupe
```

The last line only applies if you installed `0.2.4` or `0.2.6`, which put
everything in a `kubeloupe` namespace. If you would rather keep it there,
see [above](#if-you-would-rather-have-your-own-namespace) — that layout is
still supported, it just needs the third Lens setting.

Set **PROMETHEUS** back to `Auto Detect Prometheus` and clear the address
field once the default layout is in place.

A day of history stays on whichever PVC you delete. The file's magic still
reads `LMD1` and the loader still reads every version it has ever written,
so copy `snapshot.bin` across first if it is worth the trouble; otherwise
the new pod refills within the hour.

## Persistence

The store is in memory, but it is snapshotted to disk so a restart does
not cost a day of history. A **SIGTERM — every ordinary rollout — writes a
final snapshot and loses nothing**; an unclean kill costs at most
`SNAPSHOT_INTERVAL_SECONDS`.

It is a periodic snapshot rather than a write-through store on purpose.
Lens re-queries every chart once a minute over ranges up to 24h, which
means there is no small hot subset to cache — the working set is the whole
window — so reading from disk would be constant I/O to serve a few MB.
Compressing the samples was the cheaper answer by some margin: it removed
about seven eighths of the memory without putting anything in the query
path. One sequential write every few minutes keeps that path a pointer
walk into a varint stream.

The file holds the store's compressed chunks verbatim, so it is about as
small as the store is: a 673-series cluster's day of history is roughly a
megabyte, where the raw layout wrote 21 MiB of it.

Three properties matter more than the format:

- **The write is atomic** — a temporary file renamed over the target, then
  fsynced. A process killed mid-write leaves the previous good snapshot
  intact; without it, one OOM would turn into permanent data loss.
- **A bad snapshot is never fatal.** It is logged and the daemon starts
  empty. Persistence that can wedge the process into `CrashLoopBackOff` is
  worse than none.
- **Loading re-applies retention**, so a pod that was down for six hours
  does not come back with six hours of stale points.
- **Older files still load.** The layout changed when samples started
  being compressed; the magic did not, and the version field carries it.
  Upgrading in place costs no history, and the first save after it
  rewrites the file in the current layout.

Set `SNAPSHOT_PATH` to enable it and mount a volume there. The supplied
manifest includes a 64Mi PVC. Note the pod needs `fsGroup` matching
`runAsUser`, or the provisioner's root-owned directory is not writable and
every snapshot fails with `EACCES` — while the daemon otherwise runs
perfectly, so it surfaces only as history that never survives.
`readOnlyRootFilesystem: true` still applies: it covers the *root*
filesystem, and mounted volumes stay writable.

The PVC names no `storageClassName`, so it takes the cluster default. On
two kinds of cluster that is the wrong thing to leave to chance, and
[`deploy/pinned-storage`](deploy/pinned-storage) pins it instead:

```sh
kubectl apply -k https://github.com/bisand/kubeloupe//deploy/pinned-storage?ref=main
```

**More than one default.** Kubernetes allows several StorageClasses to
carry the default annotation; it then picks the most recently created and
warns. An unspecified class therefore resolves to whichever was added last
— which may be a `Retain` class that leaves the volume behind every time
the claim is deleted. Worth checking with `kubectl get sc` before
installing: more than one `(default)` is a cluster-wide latent bug, not
just one for this daemon.

**More than one node.** A node-local class such as k3s' `local-path`
strands the snapshot on whichever node first scheduled the pod, so a
reschedule quietly starts from an empty store — the daemon runs perfectly
and simply has no history, which is the same failure shape as the `EACCES`
one above. A replicated class follows the pod.

The overlay pins `longhorn`; change the value for whatever the cluster
provides. Storage class is immutable on an existing claim, so this has to
be right at creation — an already-bound PVC keeps its class no matter what
the manifest says later.

## Configuration

| variable | default | |
| --- | --- | --- |
| `SCRAPE_INTERVAL_SECONDS` | `30` | Halving it doubles the store's memory, though a regular interval is also what compresses best. |
| `RETENTION_HOURS` | `24` | Covers Lens' longest range. Rounded up to a chunk boundary, so the store holds a little over what it says. |
| `SNAPSHOT_PATH` | *(unset)* | Unset disables persistence, so the daemon runs fine with no volume attached. |
| `SNAPSHOT_INTERVAL_SECONDS` | `300` | Upper bound on history lost to an *unclean* kill. |
| `LISTEN_ADDR` | `0.0.0.0:9090` | |

RBAC is read-only: `get`/`list` on nodes and pods, `get` on `nodes/proxy`.

## Build

```sh
docker build --platform linux/amd64 -t kubeloupe:dev .
```

The builder is deliberately **not** pinned to `$BUILDPLATFORM`: it must run
as the target platform so the binary matches the node. Building on an
arm64 laptop for an amd64 node goes through emulation and takes a few
minutes — which is the correct trade against shipping a binary the node
cannot execute.

`rust:alpine` targets musl natively, so the binary is static and the
runtime image is `FROM scratch`: no base OS, nothing to patch.

The tag above is `dev` on purpose. Released images are built and tagged by
the release workflow from the git tag, so a version number written here
would be one more place to remember, and silently wrong the first time it
is forgotten.

If you would rather not use a registry at all, import straight into the
node's containerd and set `imagePullPolicy: Never`:

```sh
docker save kubeloupe:dev | ssh <node> 'sudo k3s ctr images import -'
```

## Releases

Publishing is driven by **releases**, not tags. A tag is a bookmark —
cheap to push by accident, movable while something is being prepared — so
pushing one builds nothing. Publishing a GitHub release builds the image
for both architectures and pushes it to ghcr, which means everything
published corresponds to something someone decided to ship and wrote a
changelog for.

> [!NOTE]
> A `release` event resolves the workflow from the ref the release points
> at — the tag — not from the default branch. A release published for a
> tag cut before this workflow existed cannot build itself.

## Tests

```sh
cargo test
```

The suite evaluates the real Lens query strings against a synthetic
cluster and asserts the numbers — including failure modes that would be
invisible in a chart: an unanchored pod regex silently summing `api-1`
with `api-10`, a container restart reading as a negative rate, and a
stale series flatlining instead of breaking the line.

## The name

A **loupe** is the small lens a jeweller or watchmaker holds up to one eye
to look closely at one thing. No bench, no stand, no apparatus — you carry
it in a pocket and hold it up when you need it. That is the whole design
brief: 2 MB on disk and a handful of MiB resident against a 170 MB stack,
for someone looking closely at one small cluster.

`kubeloupe` rather than plain `loupe` because that word is thoroughly
taken — a commercial APM product trades as Loupe, crates.io's belongs to
Wasmer, and npm's is a dependency of Chai. Four extra characters is a
cheap price for not being the fifth thing with the name.

No hyphen, which is not an aesthetic choice: Kubernetes reserves the
`kube-` prefix for its own namespaces, so `kube-loupe` would be squatting
on a reservation. `kubeloupe` isn't.

It does still install into a namespace called `lens-metrics`, which is the
one name here that nobody chose — see [Quick start](#quick-start). A loupe
is only useful held up to something, and Lens decides where it gets held.

## License

Apache-2.0. See [LICENSE](LICENSE).
