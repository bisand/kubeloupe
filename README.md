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

It can also keep the logs of pods you name, for as long as you say, as
plain text files that outlive the pods -- see [Logs](#logs-for-the-pods-you-name).
And it can stand in for a small Prometheus or VictoriaMetrics: scrape the
exporters you name, keep only the metrics you list, and hold months of
history on disk while memory holds an hour -- see
[History on disk, and exporters](#history-on-disk-and-exporters).
All of that is off unless configured: an installation that doesn't ask
for it reads, writes and serves none of it, and stays the few megabytes
it always was.

| | image (compressed) | resident |
| --- | ---: | ---: |
| Prometheus + node-exporter + kube-state-metrics | 141.7 MB | ~250 MB |
| VictoriaMetrics + node-exporter + kube-state-metrics | 50.6 MB | ~170 MB |
| **kubeloupe** | **2.0 MB** | **4.7-9.6 MiB** |

Samples are the entire footprint, so they are compressed. Timestamps and
values become varint delta streams, and the values are first recovered as
the integers they always were — the bytes, nanoseconds and millicores that
got divided into `f64` on the way in. Replaying a day of a real 673-series
cluster through the encoder gives **1.28 bytes per sample against 16 raw,
12x smaller**. The store costs more than the encoder does — 2.28 bytes a
sample on that same day — because the newest hour of each series is held
raw. Tests guard each figure separately, so neither can quietly drift back
toward raw.

Only the newest hour of each series is kept raw, so appends stay a pointer
bump and instant queries never decompress anything. A 5m `rate()` over a
day of history costs about 90 µs, and the worst query Lens can send — a
24h rate across 96 pods — about 1 ms.

Sample memory follows the sample count rather than the cluster:
`series x (retention / scrape interval) x ~2.3 bytes`, plus labels — the
store figure rather than the encoder's, since that is what is resident.
For the 673-series cluster above that is 4.2 MiB of samples where the raw
form held 29.6 MiB. The shape is a plateau, not a leak: retention bounds it,
and there are tests that say so.

Four clusters have now run this for a full day on each build. The raw-sample
build — image `0.2.8`, whose daemon `0.2.9` did not change — was measured on
the same four in the hour before the compression upgrade, holding sample
counts within 3% of `0.2.10`'s, so those two columns compare like with like.

| cluster | series | samples | `0.2.8` | `0.2.10` |
| --- | ---: | ---: | ---: | ---: |
| 1 node, 10 pods | 122 | 354k | 13.25 MiB | **2.63 MiB** |
| 1 node, 21 pods | 194 | 537k | 14.84 MiB | **3.07 MiB** |
| 3 nodes, 65 pods | 538 | 1.53M | 32.71 MiB | **5.79 MiB** |
| 5 nodes, 107 pods | 1247 | 2.63M | 61.77 MiB | **12.00 MiB** |

That is 4.8x to 5.6x less resident memory for the same history.

`0.2.11` sizes the head and chunk vectors exactly and refetches the pod list
every 5 minutes rather than every 30 seconds. The same four, after 24.7 hours
on it:

| cluster | series | samples | working set | RSS | bytes/sample |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 node, 10 pods | 122 | 351k | 3.56 MiB | **1.68 MiB** | 5.03 |
| 1 node, 21 pods | 423 | 553k | 5.50 MiB | **2.91 MiB** | 5.53 |
| 3 nodes, 65 pods | 530 | 1.58M | 5.66 MiB | **5.33 MiB** | 3.54 |
| 5 nodes, 107 pods | 963 | 2.79M | 9.93 MiB | **9.46 MiB** | 3.56 |

Series counts drift with what the clusters happen to be running, so bytes a
sample is what compares across a day; absolute MiB does not. `0.2.10` at 28
hours on the same four cost 7.07, 4.58, 4.62 and 4.67 bytes a sample. Three of
the four came down 21 to 29% — sizing the vectors exactly is worth about a
fifth of resident memory once a process has settled.

The 32-37% drop that showed minutes after the rollout was too good. Those were
fresh processes read against `0.2.10` figures carrying a day of allocator churn,
and roughly a third of the apparent win was the freshness rather than the code.
The matched-uptime figure above is the one to quote.

The one-node, 21-pod row does not measure the change. Its series count went
from 194 to 423 while its samples barely moved, so per-series cost spread over
the same samples pushed its bytes a sample up rather than down. The extra
series were a bug, found afterwards and fixed in `0.2.12`: **sidecars** — init
containers with `restartPolicy: Always`, which the kubelet reports as running
containers — were read from neither `spec.containers` nor
`status.containerStatuses`, so their series carried `image="unknown"` and a
second set appeared once the real image arrived. A day on `0.2.12` says the
read is right: every `plugin-barman-cloud` sidecar on the two clusters running
CloudNativePG now carries its real image, and none of the four has a series
left unnamed. What still turns up is a single sample against `image="unknown"`
when a rollout or a CronJob puts a genuinely new container on a node — one
scrape before the list is refetched, which is the window the staleness guard
is built to leave rather than a case it misses.

CPU came down with the pod list, which is the largest read this makes. The
107-pod cluster parses about 1.46 MiB of JSON a scrape, 0.89 MiB of it the pod
list; at one refetch per ten scrapes its average went from 1.479 to 0.901
millicores. The other three went 0.201 to 0.092, 0.149 to 0.097, and 0.437 to
0.275.

Two of those were read as understating the saving, on the theory that the
sidecar bug kept the pod list refetching every scrape wherever one was
running. A day of `0.2.12` does not bear that out. With the sidecars named
and the refetching stopped, the same four sit at 0.090, 0.089, 0.265 and
about 0.90 millicores — `0.2.11` again, within a few percent. The gap that
looked like refetch cost was the statistic rather than the code: these are
medians, and the figures it was compared against were means over a longer
window, which an occasional client sweeping all four clusters at once lifts
and a median does not. Naming a container costs a hash lookup either way.
`0.2.12` corrects what the series say, not what they cost.

Working set and RSS have separated on the two clusters whose snapshot volume is
a hostPath — 1.9 and 2.6 MiB apart, against 0.3 and 0.5 MiB on the two backed
by Longhorn — and that gap grew over the day while RSS stayed flat, which is
what page cache from the snapshot writes being charged to the container would
look like. RSS is the like-for-like number across all four. The range in the
table at the top is working set, which is the more pessimistic of the two.

Across the whole process that is 3.5 to 5.5 bytes a sample — the store's 1.3,
plus labels, the series index and allocator slack, spread over fewer samples on
the smaller clusters. The five-node box has come down from 25.1 bytes a sample
on `0.2.8`, to 4.8 on `0.2.10`, to 3.56. Underneath that, resident memory now
tracks series count more than sample count: the two Longhorn-backed clusters
both settled at about 10 KiB a series, and the 122-series box costs 14 KiB
because fixed process cost has fewer series to spread across.

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

What that does **not** buy is the rest of Prometheus. Beyond what Lens
generates, the PromQL here is what hand-written dashboards and alerts
mostly use: `sum`, `avg`, `min`, `max` and `count` with `by` or
`without`; `rate`, `irate` and `increase`; comparisons (`== 1`, `> 0`)
and `and`, `or`, `unless`; `clamp_min`, `clamp_max`, `vector()`,
`time()`; and arithmetic. `histogram_quantile`, `topk`, `offset`,
`on()`/`group_left` and subqueries are a 422, so a prebuilt Kubernetes
dashboard will mostly not render — it expects both those and the full
kube-state-metrics/node-exporter catalogue. Hand-written panels within the
subset work.

Two behaviours follow VictoriaMetrics rather than Prometheus, because they
draw better charts. A range window narrower than the query's step is
widened to the step, so `rate(x[1m])` over a month averages each point
rather than sampling one minute of it. And `increase()` counts from the
last sample before its window, so it is exact rather than extrapolated:
`increase(x[1h])` at whole hours adds up to the day.

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

### Logs, for the pods you name

Off unless asked for, and then only for what is named. The kubelet keeps a
container's log only while its pod exists, so a rollout or an eviction takes
the lines with it. Given `LOG_TARGETS`, kubeloupe follows those pods' logs
through the API server (`pods/log?follow=true`) and appends each line, as it
is written, to plain text files, one per target per day:

```
LOG_TARGETS=meet/meet-=14d,meet/glugg-db-=3d,kube-system
/var/lib/kubeloupe/logs/meet_meet-/2026-11-13.log
```

A target is `namespace[/pod-prefix][=retention]` (`90m`, `24h`, `7d`; 24h if
unset), and a pod belongs to the first target that names it. Pods that no
target names are never read. Each target's days are deleted once they are
older than its retention, and `LOG_MAX_MB` caps all of them together,
oldest day first (never today's). Because each container's log is followed
rather than polled, a pod's last lines are kept even when it exits a moment
after being told to stop, and a container that restarted has its previous
run read too. Every `LOG_INTERVAL_SECONDS` the pods are listed again: new
containers get a stream, and an ended one is picked up from where it got to.
Each stream is one connection through the API server, reopened every ten
minutes so a silently dead one can't hide lines.

The process holds only where it got to in each container's log, so
retention costs disk, not memory. After a restart it carries on from the
newest line on disk.

Lines are stored and served as `<time> <node> <pod>/<container> <text>`:

```
curl -H "Authorization: Bearer $TOKEN" \
  'http://prometheus.lens-metrics.svc/api/v1/logs?namespace=meet&pod=meet-&since=3600&match=error&limit=500'
```

| parameter | |
| --- | --- |
| `namespace` | Every target's if unset. |
| `pod` | A prefix of the pod's name. |
| `container`, `node` | Exact. |
| `since` | Seconds back from now; an hour if unset. |
| `match` | Text the line must contain, ignoring case. |
| `limit` | The newest this many lines (5000, at most 50000), oldest first. |

Logs say more than metrics do, and anything on the cluster can reach the
Service, so `/api/v1/logs` asks for `LOG_TOKEN` as a bearer, and logs stay
off without one. The manifest reads both from an optional Secret, the token
as a file on every request, so changing it needs no restart:

```
kubectl -n lens-metrics create secret generic kubeloupe-logs \
  --from-literal=targets=meet/meet-=30d,meet/glugg-db-=7d \
  --from-literal=token=$(openssl rand -hex 24) \
  --from-literal=max-mb=128
kubectl -n lens-metrics rollout restart deploy/kubeloupe
```

Retention and the cap are settings, not code: change the Secret and restart
the pod. Lowering a retention deletes the days past it on the next poll;
raising one keeps what is on disk and lets it grow. Size the volume for
`max-mb` plus the metrics snapshot: the supplied claim is 64Mi, enough for
the default 32. (k3s' local-path doesn't enforce a claim's size, so there
the cap is what keeps the disk in check.)

To size it, a target needs about its pods' daily log volume times its days:
`kubectl logs --since=24h <pod> | wc -c`.

### History on disk, and exporters

Off unless asked for. Two settings turn kubeloupe from Lens' backend into
a small, general one -- enough to run Grafana on, as the Glugg cluster
does in place of VictoriaMetrics:

- **`SCRAPE_CONFIG`** names Prometheus exporters to read, as JSON (inline,
  or the path of a file holding it):

  ```json
  [{ "job": "node", "every": "10s", "namespace": "monitoring",
     "selector": "app=node-exporter", "port": 9100, "instance": "node",
     "keep": "node_cpu_seconds_total|node_memory_(MemAvailable|MemTotal)_bytes|node_load1" },
   { "job": "kube-state-metrics",
     "url": "http://kube-state-metrics.monitoring.svc:8080/metrics",
     "keep": "kube_pod_status_ready|kube_node_status_condition" }]
  ```

  A job is either one `url`, or the Running pods a label `selector` finds
  (in `namespace`, or every namespace when it is left out), read at their
  pod IP on `port` -- a number or a container port's name -- and `path`
  (`/metrics`). Every series is labelled `job` and `instance`: the pod's
  `node` or `pod` name (the default), or the URL's host and port. `every`
  defaults to `SCRAPE_INTERVAL_SECONDS`. `keep` is a regex every metric
  name must match whole: it is what keeps a node-exporter's thousand
  series down to the dozen a dashboard reads, and so what keeps the store
  small. `up` is always recorded, 1 or 0 per target. Plain HTTP only.

- **`STORAGE_DIR`** keeps history on disk. Memory then holds only the
  hour in progress: as each hour turns, every series is sealed and its
  compressed chunks appended to that day's file, and memory lets go of
  them. Queries read memory and files alike. Retention is
  `STORAGE_RETENTION_DAYS` (30), and `STORAGE_MAX_MB` (1024) caps the
  whole by dropping the oldest days first. Optionally, days older than
  `STORAGE_DOWNSAMPLE_AFTER_DAYS` keep one point per
  `STORAGE_DOWNSAMPLE_SECONDS` (300): the last of each, which is what
  keeps `rate()` and `increase()` exact on a counter, at a fraction of the
  size. A finished day is merged into one segment; nothing is ever
  rewritten in place, and a rewrite goes to a temporary file renamed over
  the old one. The hour in memory is snapshotted as usual (by default to
  `head.snapshot` in the same directory), and a SIGTERM writes it to disk.

There is no database underneath, on purpose. The memory store already
packs samples into immutable compressed chunks, which is what a time
series database writes anyway; a day file is a run of those, grouped by
series, with a small table of metric names in front of each segment so a
query reads only the names it asks for and only the chunks that overlap
its range. The page cache does the caching. A general-purpose key-value
store would cost ten times the bytes per sample and still need all of
this on top.

Sizing: bytes on disk are about samples per second x 86 400 x 1.3-3.5 per
day. `/-/stats` reports `disk_bytes` and `disk_days`. The volume needs
room for `STORAGE_MAX_MB` plus a day being rewritten.

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
walk into a varint stream. (For history longer than a day, the
[disk tier](#history-on-disk-and-exporters) is the other answer: there the
working set no longer fits, and the files are read only for the range a
query asks for.)

The file holds the store's compressed chunks verbatim, so it is about as
small as the store is: a 673-series cluster's day of history is roughly
two and a half megabytes, where the raw layout wrote 21 MiB of it.

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
| `POD_REFRESH_INTERVAL_SECONDS` | `300` | The pod list is the largest read and the slowest-changing, so it has its own cadence. A container it cannot name shortens the wait to one scrape. |
| `LISTEN_ADDR` | `0.0.0.0:9090` | |
| `LOG_TARGETS` | *(unset)* | What to keep logs of, and for how long. Unset, no logs are read. See [Logs](#logs-for-the-pods-you-name). |
| `LOG_TOKEN` | *(unset)* | The bearer `/api/v1/logs` asks for. Required for logs, unless `LOG_TOKEN_FILE`. |
| `LOG_TOKEN_FILE` | *(unset)* | The bearer in a file, read on every request: a Secret mounted as a volume can be changed without a restart. Wins over `LOG_TOKEN`. The supplied manifest uses it. |
| `LOG_DIR` | `/var/lib/kubeloupe/logs` | On the snapshot volume in the supplied manifest. |
| `LOG_MAX_MB` | `32` | All targets' files together. Size the volume to match. |
| `LOG_INTERVAL_SECONDS` | `15` | How often the pods are listed: how soon a new container's log is followed. Lines themselves arrive as they are written. |
| `SCRAPE_CONFIG` | *(unset)* | Exporters to read, as JSON or a file path. Unset, none. See [History on disk, and exporters](#history-on-disk-and-exporters). |
| `STORAGE_DIR` | *(unset)* | Keep history on disk here. Unset, it stays in memory for `RETENTION_HOURS`, which `STORAGE_RETENTION_DAYS` then replaces. |
| `STORAGE_RETENTION_DAYS` | `30` | Whole days are deleted once past it. |
| `STORAGE_MAX_MB` | `1024` | All days together; the oldest go first, never today. |
| `STORAGE_DOWNSAMPLE_AFTER_DAYS` | *(unset)* | Days older than this keep one point per `STORAGE_DOWNSAMPLE_SECONDS`. Unset, every sample is kept. |
| `STORAGE_DOWNSAMPLE_SECONDS` | `300` | |

The supplied manifest reads every variable but `SNAPSHOT_PATH` from the
ConfigMap `kubeloupe-config` when there is one, one key per variable, so a
cluster keeps its settings there and the manifest stays the same. The pod
reads it only at start: after `kubectl edit` or `create`, restart it.

To have a change restart the pod by itself, keep the settings in files
with [`deploy/with-settings`](deploy/with-settings): copy the directory,
edit `kubeloupe.env`, and `kubectl apply -k .`. kustomize names the
ConfigMap after a hash of its contents and points the Deployment at that
name, so a new setting is a new pod. Flux and Argo apply it as it is.

RBAC is read-only: `get`/`list` on nodes and pods, `get` on `nodes/proxy` and `pods/log`.

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
stale series flatlining instead of breaking the line. It does the same for
every query on a real Grafana dashboard set and its alerts, and checks that
history read back from disk -- merged, downsampled, reopened after a
restart, or with a torn segment at its end -- answers exactly as memory
would.

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
