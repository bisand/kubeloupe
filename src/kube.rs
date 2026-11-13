//! The only thing this daemon talks to: the Kubernetes API server.
//!
//! Node and Pod objects supply everything kube-state-metrics would have,
//! and the kubelet's `/stats/summary` -- reached through the API server's
//! node proxy -- supplies everything node-exporter and cadvisor would.
//! Going through the proxy rather than straight to port 10250 means one
//! host, one CA, one token, and no `insecure_skip_verify` anywhere.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Client {
    http: reqwest::Client,
    /// For logs followed as they are written: no overall timeout, since a
    /// quiet container's stream rightly stays open for hours, and TCP
    /// keepalives so a dead connection is noticed anyway.
    follow: reqwest::Client,
    base: String,
    token_path: String,
}

impl Client {
    pub fn in_cluster() -> Result<Self> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .context("KUBERNETES_SERVICE_HOST is unset -- this must run inside the cluster")?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_string());

        // The ClusterIP, not the DNS name: it is in the API server's SAN
        // list, and using it means the scratch image never needs a
        // resolver to reach its only dependency.
        let base = if host.contains(':') {
            format!("https://[{host}]:{port}")
        } else {
            format!("https://{host}:{port}")
        };

        let ca = std::fs::read(Path::new(SA_DIR).join("ca.crt"))
            .context("reading the ServiceAccount CA certificate")?;

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("kubeloupe");
        let mut follow = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent("kubeloupe");

        for cert in reqwest::Certificate::from_pem_bundle(&ca)? {
            builder = builder.add_root_certificate(cert.clone());
            follow = follow.add_root_certificate(cert);
        }

        Ok(Self {
            http: builder.build()?,
            follow: follow.build()?,
            base,
            token_path: format!("{SA_DIR}/token"),
        })
    }

    /// Read on every call rather than cached: projected ServiceAccount
    /// tokens expire and are rewritten in place, and a daemon that caches
    /// one starts returning 401 an hour after it starts.
    fn token(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.token_path)?
            .trim()
            .to_string())
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token()?)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GET {path} returned {status}: {}", body.trim());
        }

        response
            .json()
            .await
            .with_context(|| format!("decoding {path}"))
    }

    pub async fn nodes(&self) -> Result<List<Node>> {
        self.get("/api/v1/nodes").await
    }

    pub async fn pods(&self) -> Result<List<Pod>> {
        self.get("/api/v1/pods").await
    }

    pub async fn pods_in(&self, namespace: &str) -> Result<List<Pod>> {
        self.get(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await
    }

    /// Pods matching a label selector, in one namespace or (empty) all.
    pub async fn pods_selected(&self, namespace: &str, selector: &str) -> Result<List<Pod>> {
        let scope = if namespace.is_empty() {
            String::new()
        } else {
            format!("/namespaces/{namespace}")
        };
        let selector: String = form_urlencoded::byte_serialize(selector.as_bytes()).collect();
        self.get(&format!("/api/v1{scope}/pods?labelSelector={selector}"))
            .await
    }

    /// A container's log with the kubelet's own timestamp on every line,
    /// from `since` (unix seconds) or, without one, the last `backlog`
    /// seconds. `previous` reads the run before the latest restart, which
    /// holds the lines a crash left behind. At most `logs::FETCH_LIMIT`
    /// bytes, the oldest first, so a long backlog is caught up over several calls.
    pub async fn container_log(
        &self,
        namespace: &str,
        pod: &str,
        container: &str,
        since: Option<i64>,
        backlog: i64,
        previous: bool,
    ) -> Result<String> {
        let limit = crate::logs::FETCH_LIMIT;
        let from = match since {
            Some(t) => format!("sinceTime={}", crate::logs::rfc3339(t * 1_000_000_000, 0)),
            None => format!("sinceSeconds={backlog}"),
        };
        let path = format!(
            "/api/v1/namespaces/{namespace}/pods/{pod}/log?container={container}&timestamps=true&limitBytes={limit}&{from}{}",
            if previous { "&previous=true" } else { "" }
        );
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token()?)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GET {path} returned {status}: {}", body.trim());
        }
        Ok(body)
    }

    /// A container's log as it is written, from `since` (unix seconds) or
    /// the last `backlog` seconds, until the container stops or the
    /// connection does. Read it with `chunk()`.
    pub async fn follow_log(
        &self,
        namespace: &str,
        pod: &str,
        container: &str,
        since: Option<i64>,
        backlog: i64,
    ) -> Result<reqwest::Response> {
        let from = match since {
            Some(t) => format!("sinceTime={}", crate::logs::rfc3339(t * 1_000_000_000, 0)),
            None => format!("sinceSeconds={backlog}"),
        };
        let path = format!(
            "/api/v1/namespaces/{namespace}/pods/{pod}/log?container={container}&timestamps=true&follow=true&{from}"
        );
        let response = self
            .follow
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token()?)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GET {path} returned {status}: {}", body.trim());
        }
        Ok(response)
    }

    pub async fn node_stats(&self, node: &str) -> Result<StatsSummary> {
        self.get(&format!("/api/v1/nodes/{node}/proxy/stats/summary"))
            .await
    }
}

#[derive(Deserialize)]
pub struct List<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
}

#[derive(Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
}

#[derive(Deserialize)]
pub struct Node {
    pub metadata: Metadata,
    #[serde(default)]
    pub status: NodeStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    #[serde(default)]
    pub capacity: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub allocatable: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct Pod {
    pub metadata: Metadata,
    #[serde(default)]
    pub spec: PodSpec,
    #[serde(default)]
    pub status: PodStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodSpec {
    #[serde(default)]
    pub node_name: String,
    #[serde(default)]
    pub containers: Vec<Container>,
    /// Sidecars live here, not in `containers`: since Kubernetes 1.28 a
    /// container that runs alongside the workload for the pod's whole life
    /// is an init container with `restartPolicy: Always`. The kubelet
    /// reports it under `containers` in stats/summary all the same, so a
    /// daemon that reads only `spec.containers` sees a running container
    /// it cannot name.
    #[serde(default)]
    pub init_containers: Vec<Container>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub resources: Resources,
    /// Only ever set on an init container, and only `Always` there, which
    /// is what distinguishes a sidecar from a plain init container.
    #[serde(default)]
    pub restart_policy: Option<String>,
    /// For scraping: a target may name its port rather than number it.
    #[serde(default)]
    pub ports: Vec<ContainerPort>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    #[serde(default)]
    pub name: String,
    pub container_port: u16,
}

impl Container {
    /// A sidecar holds its resources for the pod's whole life, so it
    /// counts toward what the pod reserves. A plain init container has
    /// finished by the time anything is scraped and does not.
    pub fn is_sidecar(&self) -> bool {
        self.restart_policy.as_deref() == Some("Always")
    }
}

#[derive(Deserialize, Default)]
pub struct Resources {
    #[serde(default)]
    pub requests: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub limits: std::collections::HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default, rename = "podIP")]
    pub pod_ip: String,
    #[serde(default)]
    pub container_statuses: Vec<ContainerStatus>,
    #[serde(default)]
    pub init_container_statuses: Vec<ContainerStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatus {
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub restart_count: u32,
}

// --- kubelet /stats/summary -------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsSummary {
    pub node: NodeStats,
    #[serde(default)]
    pub pods: Vec<PodStats>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStats {
    pub cpu: Option<CpuStats>,
    pub memory: Option<MemoryStats>,
    pub fs: Option<FsStats>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodStats {
    pub pod_ref: PodRef,
    #[serde(default)]
    pub containers: Vec<ContainerStats>,
    pub network: Option<NetworkStats>,
    #[serde(default)]
    pub volume: Vec<VolumeStats>,
}

#[derive(Deserialize)]
pub struct PodRef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStats {
    #[serde(default)]
    pub name: String,
    pub cpu: Option<CpuStats>,
    pub memory: Option<MemoryStats>,
    pub rootfs: Option<FsStats>,
    pub logs: Option<FsStats>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuStats {
    pub usage_core_nano_seconds: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    pub available_bytes: Option<f64>,
    pub working_set_bytes: Option<f64>,
    pub usage_bytes: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsStats {
    pub capacity_bytes: Option<f64>,
    pub available_bytes: Option<f64>,
    pub used_bytes: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    pub rx_bytes: Option<f64>,
    pub tx_bytes: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeStats {
    pub used_bytes: Option<f64>,
    pub capacity_bytes: Option<f64>,
    pub pvc_ref: Option<PvcRef>,
}

#[derive(Deserialize)]
pub struct PvcRef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
}

/// Kubernetes quantities: `1`, `100m`, `1990168Ki`, `2Gi`, `1e3`.
/// Written out rather than pulled in as a dependency -- it is twenty
/// lines, and the alternative brings the whole apimachinery surface.
pub fn parse_quantity(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(split);
    let number: f64 = number.parse().ok()?;

    let multiplier = match suffix {
        "" => 1.0,
        "m" => 0.001,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        exponent if exponent.starts_with('e') || exponent.starts_with('E') => {
            return Some(number * 10f64.powi(exponent[1..].parse::<i32>().ok()?));
        }
        _ => return None,
    };

    Some(number * multiplier)
}
