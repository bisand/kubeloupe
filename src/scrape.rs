//! Scraping: Prometheus `/metrics` from the targets you name, for the
//! metric names you keep. Off unless `SCRAPE_CONFIG` is set.
//!
//! `SCRAPE_CONFIG` is a JSON list, inline or in a file it names:
//!
//! ```json
//! [{ "job": "node", "every": "10s", "namespace": "monitoring",
//!    "selector": "app=node-exporter", "port": 9100, "instance": "node",
//!    "keep": "node_cpu_seconds_total|node_memory_.*" },
//!  { "job": "kube-state-metrics",
//!    "url": "http://kube-state-metrics.monitoring.svc:8080/metrics" }]
//! ```
//!
//! A job is either one `url`, or the Running pods a label `selector` finds
//! (in `namespace`, or all of them when it is empty), at their pod IP and
//! `port` -- a number, or a container port's name -- and `path`. Each
//! series gets `job` and `instance` labels: the pod's `node` or `pod` name,
//! or the URL's host and port. `keep` is a regex every metric name must
//! match, whole, to be kept: it is what keeps a node-exporter's thousand
//! series to the dozen a dashboard reads. `up` is always recorded.
//!
//! Plain HTTP only: the image carries no root certificates, and every
//! exporter in a cluster serves HTTP on the pod network anyway.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex_lite::Regex;
use serde::Deserialize;

use crate::api::Shared;
use crate::kube;
use crate::store::{Labels, NAME_LABEL};

/// How often a job's pods are listed again.
const DISCOVER_EVERY: i64 = 60;
/// A target exposing more than this is cut off there, so one exporter's
/// cardinality accident cannot take the store down with it.
const MAX_SAMPLES: usize = 50_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job: String,
    every: Option<String>,
    url: Option<String>,
    #[serde(default)]
    namespace: String,
    selector: Option<String>,
    port: Option<serde_json::Value>,
    #[serde(default = "metrics_path")]
    path: String,
    instance: Option<String>,
    keep: Option<String>,
}

fn metrics_path() -> String {
    "/metrics".to_string()
}

/// A job ready to run.
pub struct Plan {
    job: Job,
    every: i64,
    keep: Option<Regex>,
}

impl Plan {
    pub fn describe(&self) -> String {
        format!("{} every {}s", self.job.job, self.every)
    }
}

/// The jobs from `SCRAPE_CONFIG`, checked; `None` when it is unset.
/// `default_every` is the scrape interval of the built-in collection.
pub fn from_env(default_every: i64) -> Result<Option<Vec<Plan>>> {
    let Some(spec) = std::env::var("SCRAPE_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(None);
    };
    let text = if spec.trim_start().starts_with('[') {
        spec
    } else {
        std::fs::read_to_string(spec.trim()).with_context(|| format!("reading {spec}"))?
    };
    parse_config(&text, default_every).map(Some)
}

pub fn parse_config(text: &str, default_every: i64) -> Result<Vec<Plan>> {
    let jobs: Vec<Job> = serde_json::from_str(text).context("SCRAPE_CONFIG")?;
    jobs.into_iter()
        .map(|job| {
            let every = match &job.every {
                Some(every) => crate::logs::parse_duration(every)
                    .filter(|s| *s > 0)
                    .with_context(|| format!("job {}: bad interval {every:?}", job.job))?,
                None => default_every,
            };
            match (&job.url, &job.selector) {
                (Some(url), None) if url.starts_with("http://") => {}
                (Some(_), None) => bail!("job {}: only http:// URLs can be scraped", job.job),
                (None, Some(_)) if job.port.is_some() => {}
                (None, Some(_)) => bail!("job {}: a selector needs a port", job.job),
                _ => bail!("job {}: give either a url or a selector", job.job),
            }
            if !matches!(job.instance.as_deref(), None | Some("node" | "pod")) {
                bail!("job {}: instance is \"node\" or \"pod\"", job.job);
            }
            let keep = job
                .keep
                .as_deref()
                .map(|k| Regex::new(&format!("^(?:{k})$")))
                .transpose()
                .with_context(|| format!("job {}: bad keep regex", job.job))?;
            Ok(Plan { job, every, keep })
        })
        .collect()
}

pub async fn run(client: kube::Client, plans: Vec<Plan>, store: Shared) {
    let client = Arc::new(client);
    for plan in plans {
        tokio::spawn(run_job(Arc::clone(&client), plan, store.clone()));
    }
}

/// One scrape target: where, and the `instance` it is known by.
#[derive(Clone)]
struct Target {
    url: String,
    instance: String,
}

async fn run_job(client: Arc<kube::Client>, plan: Plan, store: Shared) {
    let timeout = Duration::from_secs(plan.every.clamp(1, 10) as u64);
    let http = match reqwest::Client::builder()
        .timeout(timeout)
        .user_agent("kubeloupe")
        .build()
    {
        Ok(http) => http,
        Err(error) => {
            eprintln!("kubeloupe: scrape job {}: {error:#}", plan.job.job);
            return;
        }
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(plan.every as u64));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut targets: Vec<Target> = Vec::new();
    let mut listed = i64::MIN;

    loop {
        ticker.tick().await;
        let now = crate::now();
        if now - listed >= DISCOVER_EVERY {
            match discover(&client, &plan.job).await {
                Ok(found) => {
                    targets = found;
                    listed = now;
                }
                Err(error) => eprintln!("kubeloupe: scrape job {}: {error:#}", plan.job.job),
            }
        }

        let mut set = tokio::task::JoinSet::new();
        for target in targets.clone() {
            let http = http.clone();
            set.spawn(async move {
                let body = fetch(&http, &target.url).await;
                (target, body)
            });
        }
        let mut batch: Vec<(Labels, f64)> = Vec::new();
        while let Some(Ok((target, body))) = set.join_next().await {
            let up = match body {
                Ok(body) => {
                    keep(&plan, &target, &body, &mut batch);
                    1.0
                }
                Err(_) => 0.0,
            };
            batch.push((target_labels("up", &plan.job.job, &target.instance), up));
        }

        let mut guard = store.write().await;
        for (labels, value) in batch {
            guard.append(labels, now, value);
        }
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<String> {
    let response = http
        .get(url)
        .header("Accept", "text/plain;version=0.0.4")
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    Ok(response.text().await?)
}

fn target_labels(name: &str, job: &str, instance: &str) -> Labels {
    let mut labels = Labels::new();
    labels.insert(NAME_LABEL.to_string(), name.to_string());
    labels.insert("job".to_string(), job.to_string());
    labels.insert("instance".to_string(), instance.to_string());
    labels
}

/// The kept samples of one scrape, labelled with the target's job and
/// instance -- which win over any the exporter sets itself.
fn keep(plan: &Plan, target: &Target, body: &str, out: &mut Vec<(Labels, f64)>) {
    let mut kept = 0;
    for (name, pairs, value) in parse_exposition(body) {
        if plan.keep.as_ref().is_some_and(|k| !k.is_match(name)) {
            continue;
        }
        kept += 1;
        if kept > MAX_SAMPLES {
            eprintln!(
                "kubeloupe: {} exposes over {MAX_SAMPLES} series; the rest are dropped (narrow `keep`)",
                target.url
            );
            break;
        }
        let mut labels = target_labels(name, &plan.job.job, &target.instance);
        for (k, v) in pairs {
            if !v.is_empty() && k != "job" && k != "instance" {
                labels.insert(k, v);
            }
        }
        out.push((labels, value));
    }
}

async fn discover(client: &kube::Client, job: &Job) -> Result<Vec<Target>> {
    if let Some(url) = &job.url {
        let instance = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        return Ok(vec![Target {
            url: url.clone(),
            instance,
        }]);
    }
    let selector = job.selector.as_deref().unwrap_or_default();
    let pods = client.pods_selected(&job.namespace, selector).await?;
    let mut out = Vec::new();
    for pod in pods.items {
        if pod.status.phase != "Running" || pod.status.pod_ip.is_empty() {
            continue;
        }
        let port = match &job.port {
            Some(serde_json::Value::Number(n)) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
            Some(serde_json::Value::String(name)) => pod
                .spec
                .containers
                .iter()
                .flat_map(|c| &c.ports)
                .find(|p| &p.name == name)
                .map(|p| p.container_port),
            _ => None,
        };
        let Some(port) = port else { continue };
        let ip = &pod.status.pod_ip;
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.clone()
        };
        out.push(Target {
            url: format!("http://{host}:{port}{}", job.path),
            instance: match job.instance.as_deref() {
                Some("node") => pod.spec.node_name.clone(),
                _ => pod.metadata.name.clone(),
            },
        });
    }
    Ok(out)
}

/// One exposed sample: name, labels, value.
pub type Exposed<'a> = (&'a str, Vec<(String, String)>, f64);

/// The Prometheus text format: `name{label="value",...} value [timestamp]`
/// per line, `#` comments. Timestamps are ignored -- a sample is recorded
/// at the scrape that read it. A line that does not parse is skipped.
pub fn parse_exposition(body: &str) -> Vec<Exposed<'_>> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            parse_line(line)
        })
        .collect()
}

fn parse_line(line: &str) -> Option<Exposed<'_>> {
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let name = &line[..name_end];
    let mut rest = &line[name_end..];
    let mut labels = Vec::new();

    if let Some(inner) = rest.strip_prefix('{') {
        let mut chars = inner.char_indices().peekable();
        loop {
            // Skip separators up to a label name or the closing brace.
            while let Some(&(_, c)) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }
            let (start, c) = *chars.peek()?;
            if c == '}' {
                rest = &inner[start + 1..];
                break;
            }
            let eq = start + inner[start..].find('=')?;
            let key = inner[start..eq].trim().to_string();
            while chars.peek().is_some_and(|&(i, _)| i <= eq) {
                chars.next();
            }
            if chars.next()?.1 != '"' {
                return None;
            }
            let mut value = String::new();
            loop {
                match chars.next()?.1 {
                    '"' => break,
                    '\\' => match chars.next()?.1 {
                        'n' => value.push('\n'),
                        other => value.push(other),
                    },
                    other => value.push(other),
                }
            }
            labels.push((key, value));
        }
    }

    let value = rest.split_whitespace().next()?;
    let value: f64 = match value {
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        other => other.parse().ok()?,
    };
    Some((name, labels, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_text_format() {
        let body = "# HELP x y\n# TYPE glugg_rooms gauge\nglugg_rooms 3\n\
            node_cpu_seconds_total{cpu=\"0\",mode=\"idle\"} 123.5 1700000000000\n\
            weird{a=\"q\\\"uote\\\\d\", b=\"x,y\",} NaN\n\
            h_bucket{le=\"+Inf\"} 7\nbroken{a=\"x} 1\n\n";
        let parsed = parse_exposition(body);
        assert_eq!(parsed.len(), 4, "{parsed:?}");
        assert_eq!(parsed[0], ("glugg_rooms", vec![], 3.0));
        assert_eq!(
            parsed[1].1,
            vec![("cpu".into(), "0".into()), ("mode".into(), "idle".into())]
        );
        assert_eq!(parsed[1].2, 123.5);
        assert_eq!(
            parsed[2].1,
            vec![
                ("a".into(), "q\"uote\\d".into()),
                ("b".into(), "x,y".into())
            ]
        );
        assert!(parsed[2].2.is_nan());
        assert_eq!(parsed[3].1, vec![("le".into(), "+Inf".into())]);
    }

    #[test]
    fn keeps_only_named_metrics_and_labels_them_by_target() {
        let plans = parse_config(
            r#"[{"job":"node","selector":"app=x","port":9100,"instance":"node","keep":"node_load1|up"}]"#,
            30,
        )
        .unwrap();
        let target = Target {
            url: "http://10.0.0.1:9100/metrics".into(),
            instance: "box-1".into(),
        };
        let mut out = Vec::new();
        keep(
            &plans[0],
            &target,
            "node_load1{instance=\"spoof\"} 0.5\nnode_load15 0.1\nnode_load1x 2\n",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.get("instance").map(String::as_str), Some("box-1"));
        assert_eq!(out[0].0.get("job").map(String::as_str), Some("node"));
        assert_eq!(plans[0].every, 30);
    }

    #[test]
    fn a_bad_job_is_refused_at_start() {
        for bad in [
            r#"[{"job":"a"}]"#,
            r#"[{"job":"a","url":"https://x/metrics"}]"#,
            r#"[{"job":"a","selector":"app=x"}]"#,
            r#"[{"job":"a","url":"http://x","every":"soon"}]"#,
            r#"[{"job":"a","url":"http://x","keep":"("}]"#,
            r#"[{"job":"a","url":"http://x","instance":"host"}]"#,
            r#"[{"job":"a","url":"http://x","typo":1}]"#,
        ] {
            assert!(parse_config(bad, 30).is_err(), "{bad} should be refused");
        }
        assert!(parse_config(r#"[{"job":"a","url":"http://x:1/m","every":"5s"}]"#, 30).is_ok());
    }
}
