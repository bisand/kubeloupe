//! Container logs for the targets named in `LOG_TARGETS`, each kept as
//! long as it says, in plain text files, and served at `/api/v1/logs`.
//!
//! The kubelet keeps a container's log only while its pod exists, so a
//! rollout, an eviction or a deleted pod takes the lines with it -- often
//! the very lines someone wants to read afterwards. This follows each
//! container's log through the API server (`pods/log?follow=true`) and
//! appends lines to a file per target per day as they are written, up to
//! the moment the container stops -- a pod that exits a second after it is
//! told to still has its last words kept. Every `LOG_INTERVAL_SECONDS` the
//! pods are listed, new containers get a stream, and one that ended (or has
//! run ten minutes, so a silently dead connection can't hide lines) is
//! started again from where it got to. A container that restarted has its
//! previous run read too.
//!
//! A target is `namespace[/pod-prefix][=retention]`, such as
//! `meet/meet-=14d`: only pods it names are read, and its files are deleted
//! a whole day at a time once older than its retention (24h if unset).
//! `LOG_MAX_MB` caps all of them together, oldest day first. Without
//! `LOG_TARGETS` nothing is read, written or served, so an installation that
//! doesn't ask for logs costs nothing for them.
//!
//! Files rather than memory: retention is the user's to choose, and a week
//! of a chatty database would not fit the memory this daemon is sized for.
//! The process holds only where it got to in each container's log. The
//! files are the format the endpoint serves -- `<time> <node>
//! <pod>/<container> <text>` -- so they can be read and grepped as they are.
//! Each poll appends a container's lines in one go, so a day's file is in
//! time order per container but not across them; a query sorts.
//!
//! Logs say more than metrics do -- addresses, errors, sometimes queries --
//! and anything on the cluster can reach the Service, so the endpoint asks
//! for `LOG_TOKEN` as a bearer, and logs stay off without one. Text rather
//! than JSON: it is read by people and pasted into language models, and
//! both do better with lines.

use crate::kube::Client;
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// One read's ceiling. The kubelet returns the oldest bytes first, so a
/// long backlog is caught up over several reads instead of in one spike.
pub const FETCH_LIMIT: usize = 1 << 20;
const MAX_QUERY_LINES: usize = 50_000;
const DAY: i64 = 86_400;
const NANOS: i64 = 1_000_000_000;

pub struct Target {
    pub namespace: String,
    /// Empty for every pod in the namespace.
    pub prefix: String,
    /// Seconds.
    pub retention: i64,
    dir: PathBuf,
}

impl Target {
    fn matches(&self, namespace: &str, pod: &str) -> bool {
        self.namespace == namespace && pod.starts_with(&self.prefix)
    }

    fn file(&self, day: i64) -> PathBuf {
        self.dir
            .join(format!("{}.log", &rfc3339(day * DAY * NANOS, 0)[..10]))
    }
}

pub struct Config {
    pub targets: Vec<Target>,
    pub token: String,
    pub interval: u64,
    pub max_bytes: u64,
    pub dir: PathBuf,
}

impl Config {
    /// None when logs are off, saying why if they were asked for.
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var("LOG_TARGETS").unwrap_or_default();
        if spec.trim().is_empty() {
            return None;
        }
        let token = std::env::var("LOG_TOKEN")
            .unwrap_or_default()
            .trim()
            .to_string();
        if token.is_empty() {
            eprintln!("kubeloupe: LOG_TARGETS is set but LOG_TOKEN isn't; not collecting logs");
            return None;
        }
        let dir = PathBuf::from(
            std::env::var("LOG_DIR").unwrap_or_else(|_| "/var/lib/kubeloupe/logs".to_string()),
        );
        let targets = match parse_targets(&spec, &dir) {
            Ok(targets) => targets,
            Err(error) => {
                eprintln!("kubeloupe: LOG_TARGETS: {error:#}; not collecting logs");
                return None;
            }
        };
        for target in &targets {
            if let Err(error) = std::fs::create_dir_all(&target.dir) {
                eprintln!(
                    "kubeloupe: can't make {}: {error}; not collecting logs",
                    target.dir.display()
                );
                return None;
            }
        }
        Some(Self {
            targets,
            token,
            interval: crate::env_or::<u64>("LOG_INTERVAL_SECONDS", 15).max(1),
            max_bytes: crate::env_or::<u64>("LOG_MAX_MB", 32).max(1) << 20,
            dir,
        })
    }
}

/// `meet/meet-=14d, meet/glugg-db-=3d, kube-system`. A pod goes to the first
/// target that names it.
pub fn parse_targets(spec: &str, root: &Path) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (what, retention) = match item.split_once('=') {
            Some((what, keep)) => match parse_duration(keep.trim()) {
                Some(secs) if secs > 0 => (what.trim(), secs),
                _ => bail!("{item}: {keep} is not a duration such as 90m, 24h or 7d"),
            },
            None => (item, DAY),
        };
        let (namespace, prefix) = what.split_once('/').unwrap_or((what, ""));
        // A namespace is a DNS label, so no dots: `..` can't climb out of
        // LOG_DIR. A prefix may have them (pod names do) but only ever
        // follows `<namespace>_` in a path.
        let label = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if namespace.is_empty()
            || !label(namespace)
            || !prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            bail!("{item}: expected namespace[/pod-prefix][=retention]");
        }
        let name = if prefix.is_empty() {
            namespace.to_string()
        } else {
            format!("{namespace}_{prefix}")
        };
        if targets.iter().any(|t: &Target| t.dir == root.join(&name)) {
            bail!("{item} is named twice");
        }
        targets.push(Target {
            namespace: namespace.into(),
            prefix: prefix.into(),
            retention,
            dir: root.join(name),
        });
    }
    Ok(targets)
}

/// `30s`, `90m`, `24h`, `7d`, or a bare number of hours. Seconds.
pub fn parse_duration(s: &str) -> Option<i64> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: i64 = s[..split].parse().ok()?;
    let unit = match &s[split..] {
        "s" => 1,
        "m" => 60,
        "" | "h" => 3600,
        "d" => DAY,
        _ => return None,
    };
    n.checked_mul(unit)
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    namespace: String,
    pod: String,
    container: String,
}

/// Where the collector got to in one container's log.
#[derive(Default)]
struct Cursor {
    last: i64,
    /// The restart count when its stream last started; None until then.
    restarts: Option<u32>,
    /// A stream is reading it now.
    following: bool,
}

/// The kubelet's `timestamps=true` output, `<RFC 3339> <text>` per line, as
/// (unix nanoseconds, text). A body cut short by `limitBytes` ends mid-line;
/// that line is left for the next read, which starts from the last whole
/// one -- unless it is the only line, when keeping it is what moves on.
fn parse_body(body: &str) -> Vec<(i64, &str)> {
    let mut lines: Vec<&str> = body.split('\n').collect();
    if lines.len() > 1 {
        lines.pop();
    }
    lines
        .into_iter()
        .filter_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (stamp, text) = line.split_once(' ')?;
            Some((parse_rfc3339(stamp)?, text))
        })
        .collect()
}

/// Appends lines to the target's file for each line's day.
fn append(target: &Target, lines: &[(i64, String)]) -> Result<()> {
    let mut i = 0;
    while i < lines.len() {
        let day = lines[i].0.div_euclid(DAY * NANOS);
        let path = target.file(day);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut chunk = String::new();
        while i < lines.len() && lines[i].0.div_euclid(DAY * NANOS) == day {
            chunk.push_str(&lines[i].1);
            chunk.push('\n');
            i += 1;
        }
        file.write_all(chunk.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// A target's files, oldest first, with their day and size.
fn files(target: &Target) -> Vec<(i64, PathBuf, u64)> {
    let mut found: Vec<(i64, PathBuf, u64)> = std::fs::read_dir(&target.dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let date = name.strip_suffix(".log")?;
            let t = parse_rfc3339(&format!("{date}T00:00:00Z"))?;
            Some((
                t / (DAY * NANOS),
                entry.path(),
                entry.metadata().ok()?.len(),
            ))
        })
        .collect();
    found.sort();
    found
}

/// The newest time in a target's newest file: everything up to it was
/// read before the last restart, so a container met for the first time
/// since then starts from here rather than reading its backlog twice.
fn newest(target: &Target) -> Option<i64> {
    let (_, path, _) = files(target).pop()?;
    let file = std::fs::File::open(path).ok()?;
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| parse_rfc3339(line.split(' ').next()?))
        .max()
}

/// Deletes days past each target's retention, then the oldest day anywhere
/// (never today's) until all of them fit `max_bytes`. `now` is unix seconds.
fn prune(config: &Config, now: i64) {
    let today = now.div_euclid(DAY);
    let mut kept: Vec<(i64, PathBuf, u64)> = Vec::new();
    for target in &config.targets {
        for (day, path, size) in files(target) {
            if (day + 1) * DAY <= now - target.retention {
                let _ = std::fs::remove_file(&path);
            } else {
                kept.push((day, path, size));
            }
        }
    }
    kept.sort();
    let mut total: u64 = kept.iter().map(|(.., size)| size).sum();
    for (day, path, size) in kept {
        if total <= config.max_bytes || day >= today {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

#[derive(Deserialize, Default)]
pub struct Filter {
    /// Every target's if unset.
    pub namespace: Option<String>,
    /// A prefix of the pod's name, such as `meet-`.
    pub pod: Option<String>,
    pub container: Option<String>,
    pub node: Option<String>,
    /// Seconds back from now; an hour if unset.
    pub since: Option<i64>,
    /// Case-insensitive text the line must contain.
    #[serde(rename = "match")]
    pub matches: Option<String>,
    /// The newest this many lines.
    pub limit: Option<usize>,
}

/// The matching lines, oldest first, with the time to the millisecond.
pub fn query(config: &Config, filter: &Filter, now: i64) -> Vec<String> {
    let from = (now - filter.since.unwrap_or(3600).max(0)).saturating_mul(NANOS);
    let needle = filter
        .matches
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(str::to_lowercase);
    let limit = filter.limit.unwrap_or(5_000).clamp(1, MAX_QUERY_LINES);
    let mut hits: Vec<(i64, String)> = Vec::new();
    for target in &config.targets {
        if filter
            .namespace
            .as_deref()
            .is_some_and(|n| n != target.namespace)
        {
            continue;
        }
        for (day, path, _) in files(target) {
            if (day + 1) * DAY * NANOS <= from {
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let mut fields = line.splitn(4, ' ');
                let (Some(stamp), Some(node), Some(source), Some(text)) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let Some(t) = parse_rfc3339(stamp) else {
                    continue;
                };
                let (pod, container) = source.split_once('/').unwrap_or((source, ""));
                if t < from
                    || filter.pod.as_deref().is_some_and(|p| !pod.starts_with(p))
                    || filter.container.as_deref().is_some_and(|c| c != container)
                    || filter.node.as_deref().is_some_and(|n| n != node)
                    || needle
                        .as_deref()
                        .is_some_and(|n| !text.to_lowercase().contains(n))
                {
                    continue;
                }
                hits.push((t, format!("{} {node} {source} {text}", rfc3339(t, 3))));
                // Bounded while reading, not after: a week of a busy
                // target must not be held whole to return its last lines.
                if hits.len() >= 2 * limit {
                    newest_only(&mut hits, limit);
                }
            }
        }
    }
    newest_only(&mut hits, limit);
    hits.into_iter().map(|(_, line)| line).collect()
}

fn newest_only(hits: &mut Vec<(i64, String)>, limit: usize) {
    hits.sort_by_key(|(t, _)| *t);
    let skip = hits.len().saturating_sub(limit);
    hits.drain(..skip);
}

type Cursors = Arc<std::sync::Mutex<HashMap<Key, Cursor>>>;

pub async fn run(client: Client, config: Arc<Config>) {
    let client = Arc::new(client);
    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let floors: Vec<Option<i64>> = config.targets.iter().map(newest).collect();
    let cursors: Cursors = Arc::default();
    // The newest error a stream ended with, said with the loop's own.
    let failed: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    // Said once per new error, not once per pass.
    let mut said = String::new();
    loop {
        ticker.tick().await;
        let mut error = None;
        let mut namespaces: Vec<&str> = config
            .targets
            .iter()
            .map(|t| t.namespace.as_str())
            .collect();
        namespaces.dedup();
        let mut seen = Vec::new();
        for namespace in namespaces {
            let pods = match client.pods_in(namespace).await {
                Ok(pods) => pods,
                Err(e) => {
                    error.get_or_insert(format!("{e:#}"));
                    continue;
                }
            };
            for pod in &pods.items {
                let name = &pod.metadata.name;
                let Some(at) = config
                    .targets
                    .iter()
                    .position(|t| t.matches(namespace, name))
                else {
                    continue;
                };
                let statuses = pod
                    .status
                    .container_statuses
                    .iter()
                    .chain(&pod.status.init_container_statuses);
                for status in statuses {
                    let key = Key {
                        namespace: namespace.into(),
                        pod: name.clone(),
                        container: status.name.clone(),
                    };
                    seen.push(key.clone());
                    // One stream per container: start one unless it has one.
                    let start = {
                        let mut held = cursors.lock().unwrap();
                        let cursor = held.entry(key.clone()).or_insert_with(|| Cursor {
                            last: floors[at].unwrap_or(i64::MIN),
                            ..Cursor::default()
                        });
                        !std::mem::replace(&mut cursor.following, true)
                    };
                    if start {
                        let node = if pod.spec.node_name.is_empty() {
                            "-".to_string()
                        } else {
                            pod.spec.node_name.clone()
                        };
                        tokio::spawn(follow(
                            Arc::clone(&client),
                            Arc::clone(&config),
                            at,
                            key,
                            node,
                            status.restart_count,
                            Arc::clone(&cursors),
                            Arc::clone(&failed),
                        ));
                    }
                }
            }
        }
        // A pod that is gone has nothing more to read once its stream has
        // ended. (A failed listing leaves its namespace's cursors alone.)
        if error.is_none() {
            cursors
                .lock()
                .unwrap()
                .retain(|key, c| c.following || seen.contains(key));
        }
        if let Some(e) = failed.lock().unwrap().take() {
            error.get_or_insert(e);
        }
        match error {
            Some(e) if e != said => {
                eprintln!("kubeloupe: reading logs: {e}");
                said = e;
            }
            Some(_) => {}
            None => said.clear(),
        }
        prune(&config, crate::now());
    }
}

/// Follows one container's log until it stops, the connection does, or ten
/// minutes pass, then hands the container back to the loop, which starts
/// again from where this got to if the container is still there.
#[allow(clippy::too_many_arguments)]
async fn follow(
    client: Arc<Client>,
    config: Arc<Config>,
    at: usize,
    key: Key,
    node: String,
    restart_count: u32,
    cursors: Cursors,
    failed: Arc<std::sync::Mutex<Option<String>>>,
) {
    let result = follow_once(
        &client,
        &config.targets[at],
        &key,
        &node,
        restart_count,
        &cursors,
    )
    .await;
    if let Some(cursor) = cursors.lock().unwrap().get_mut(&key) {
        cursor.following = false;
    }
    if let Err(e) = result {
        *failed.lock().unwrap() = Some(format!("{e:#}"));
    }
}

async fn follow_once(
    client: &Client,
    target: &Target,
    key: &Key,
    node: &str,
    restart_count: u32,
    cursors: &Cursors,
) -> Result<()> {
    let (mut last, restarts) = {
        let held = cursors.lock().unwrap();
        held.get(key)
            .map_or((i64::MIN, None), |c| (c.last, c.restarts))
    };
    let save = |last: i64| {
        if let Some(cursor) = cursors.lock().unwrap().get_mut(key) {
            cursor.last = last;
        }
    };
    // sinceTime has whole seconds; lines repeated within that second are
    // no newer than the last and are dropped by keep().
    let since = (last > i64::MIN).then(|| last.div_euclid(NANOS));
    if restarts.is_some_and(|seen| restart_count > seen) && since.is_some() {
        // What the run before the restart wrote after this last read it.
        // It may be gone already; its loss is not an error.
        if let Ok(body) = client
            .container_log(
                &key.namespace,
                &key.pod,
                &key.container,
                since,
                target.retention,
                true,
            )
            .await
        {
            keep(target, key, node, &body, &mut last)?;
            save(last);
        }
    }
    if let Some(cursor) = cursors.lock().unwrap().get_mut(key) {
        cursor.restarts = Some(restart_count);
    }
    let since = (last > i64::MIN).then(|| last.div_euclid(NANOS));
    let mut response = match client
        .follow_log(
            &key.namespace,
            &key.pod,
            &key.container,
            since,
            target.retention,
        )
        .await
    {
        Ok(response) => response,
        // A container still waiting to start has no log yet: 400, and nothing wrong.
        Err(e) if e.to_string().contains("400 Bad Request") => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut pending: Vec<u8> = Vec::new();
    // Ten minutes at most: a connection that died without a word can't
    // hold a container's lines for longer than that.
    let streamed = tokio::time::timeout(Duration::from_secs(600), async {
        while let Some(chunk) = response.chunk().await? {
            pending.extend_from_slice(&chunk);
            if let Some(end) = pending.iter().rposition(|b| *b == b'\n') {
                let whole: Vec<u8> = pending.drain(..=end).collect();
                keep(
                    target,
                    key,
                    node,
                    &String::from_utf8_lossy(&whole),
                    &mut last,
                )?;
                save(last);
            }
        }
        // The container stopped: whatever it wrote last, even without a newline.
        if !pending.is_empty() {
            pending.push(b'\n');
            keep(
                target,
                key,
                node,
                &String::from_utf8_lossy(&pending),
                &mut last,
            )?;
            save(last);
        }
        anyhow::Ok(())
    })
    .await;
    match streamed {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

/// Appends the lines of `body` newer than `last`, and moves `last` on.
fn keep(target: &Target, key: &Key, node: &str, body: &str, last: &mut i64) -> Result<()> {
    let mut lines = Vec::new();
    for (t, text) in parse_body(body) {
        if t > *last {
            *last = t;
            lines.push((
                t,
                format!(
                    "{} {node} {}/{} {text}",
                    rfc3339(t, 9),
                    key.pod,
                    key.container
                ),
            ));
        }
    }
    append(target, &lines)
}

pub fn router(config: Arc<Config>) -> Router {
    Router::new()
        .route("/api/v1/logs", get(serve))
        .with_state(config)
}

async fn serve(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !same(given.as_bytes(), config.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            "a bearer token is needed for logs\n",
        )
            .into_response();
    }
    // Reading files: off the runtime's one thread.
    let lines = tokio::task::spawn_blocking(move || query(&config, &filter, crate::now()))
        .await
        .unwrap_or_default();
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// Compared in full whatever the first differing byte, so the token can't
/// be guessed a byte at a time from response times.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// --- RFC 3339, without a date crate -----------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-11-13T10:00:00.123456789Z` (any number of fraction digits, or
/// none; `Z` or `±hh:mm`) to unix nanoseconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let kept = &frac[..digits.min(9)];
        nanos = kept.parse::<i64>().ok()? * 10i64.pow(9 - kept.len() as u32);
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6
            && matches!(rest.as_bytes()[0], b'+' | b'-')
            && rest.as_bytes()[3] == b':' =>
        {
            let secs =
                rest[1..3].parse::<i64>().ok()? * 3600 + rest[4..6].parse::<i64>().ok()? * 60;
            if rest.starts_with('-') { -secs } else { secs }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * DAY + h * 3600 + mi * 60 + se - offset;
    Some(secs * NANOS + nanos)
}

/// Unix nanoseconds as UTC, with `digits` (0-9) of the fraction.
pub fn rfc3339(nanos: i64, digits: u32) -> String {
    let secs = nanos.div_euclid(NANOS);
    let frac = nanos.rem_euclid(NANOS);
    let (y, m, d) = civil_from_days(secs.div_euclid(DAY));
    let s = secs.rem_euclid(DAY);
    let base = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    );
    if digits == 0 {
        format!("{base}Z")
    } else {
        let digits = digits.min(9);
        format!(
            "{base}.{:0w$}Z",
            frac / 10i64.pow(9 - digits),
            w = digits as usize
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-13T10:00:00";
    // 2026-11-13T10:00:00Z, checked against `date -u -j -f ... +%s`.
    const T0_SECS: i64 = 1_794_564_000;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kubeloupe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config(root: &Path, spec: &str, max_bytes: u64) -> Config {
        let targets = parse_targets(spec, root).unwrap();
        for t in &targets {
            std::fs::create_dir_all(&t.dir).unwrap();
        }
        Config {
            targets,
            token: "t".into(),
            interval: 15,
            max_bytes,
            dir: root.into(),
        }
    }

    fn line(secs: i64, node: &str, pod: &str, text: &str) -> (i64, String) {
        let t = secs * NANOS;
        (t, format!("{} {node} {pod}/c {text}", rfc3339(t, 9)))
    }

    #[test]
    fn reads_and_writes_rfc3339() {
        assert_eq!(parse_rfc3339(&format!("{T0}Z")), Some(T0_SECS * NANOS));
        assert_eq!(
            parse_rfc3339(&format!("{T0}.5Z")),
            Some(T0_SECS * NANOS + 500_000_000)
        );
        assert_eq!(
            parse_rfc3339(&format!("{T0}.123456789Z")),
            Some(T0_SECS * NANOS + 123_456_789)
        );
        assert_eq!(
            parse_rfc3339(&format!("{T0}+01:00")),
            Some((T0_SECS - 3600) * NANOS)
        );
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2024-02-29T23:59:59Z"),
            Some(1_709_251_199 * NANOS)
        );
        assert_eq!(parse_rfc3339("not a time at all, no"), None);
        assert_eq!(parse_rfc3339(&format!("{T0}.Z")), None);
        assert_eq!(
            rfc3339(T0_SECS * NANOS + 123_456_789, 3),
            format!("{T0}.123Z")
        );
        assert_eq!(rfc3339(T0_SECS * NANOS, 0), format!("{T0}Z"));
        for t in [0, 951_782_400, 1_709_251_199, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&rfc3339(t * NANOS, 0)), Some(t * NANOS));
        }
    }

    #[test]
    fn reads_targets_and_durations() {
        let root = Path::new("/logs");
        let t = parse_targets("meet/meet-=14d, meet/glugg-db-=36h ,kube-system", root).unwrap();
        let got: Vec<(&str, &str, i64, PathBuf)> = t
            .iter()
            .map(|t| {
                (
                    t.namespace.as_str(),
                    t.prefix.as_str(),
                    t.retention,
                    t.dir.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("meet", "meet-", 14 * DAY, root.join("meet_meet-")),
                ("meet", "glugg-db-", 36 * 3600, root.join("meet_glugg-db-")),
                ("kube-system", "", DAY, root.join("kube-system")),
            ]
        );
        assert_eq!(parse_duration("90m"), Some(5400));
        assert_eq!(parse_duration("2"), Some(7200));
        assert_eq!(parse_duration("1w"), None);
        for bad in ["meet=forever", "../etc", "meet/../x", "meet=0d", "a,a"] {
            assert!(parse_targets(bad, root).is_err(), "{bad}");
        }
    }

    #[test]
    fn keeps_whole_lines_and_leaves_a_cut_one() {
        let body = format!("{T0}.1Z first\n{T0}.2Z second\n{T0}.3Z thi");
        let got = parse_body(&body);
        assert_eq!(
            got,
            [
                (T0_SECS * NANOS + 100_000_000, "first"),
                (T0_SECS * NANOS + 200_000_000, "second")
            ]
        );
        // A line longer than one read still moves on.
        assert_eq!(parse_body(&format!("{T0}Z a very long li")).len(), 1);
        assert!(parse_body("").is_empty());
    }

    #[test]
    fn files_by_day_queried_merged_and_filtered() {
        let root = dir("query");
        let cfg = config(&root, "meet/meet-=7d,meet/glugg-db-=2d", 1 << 20);
        let (meet, db) = (&cfg.targets[0], &cfg.targets[1]);
        // Across midnight: two files for meet.
        let midnight = (T0_SECS / DAY + 1) * DAY;
        append(
            meet,
            &[
                line(midnight - 5, "hel1-1", "meet-a", "Directory: reset"),
                line(midnight + 5, "fsn1-1", "meet-b", "directory: ok"),
            ],
        )
        .unwrap();
        append(
            db,
            &[line(
                midnight - 1,
                "hel1-2",
                "glugg-db-1",
                "checkpoint starting",
            )],
        )
        .unwrap();
        assert_eq!(files(meet).len(), 2);
        let now = midnight + 10;
        let texts = |f: Filter| -> Vec<String> {
            query(&cfg, &f, now)
                .iter()
                .map(|l| l.splitn(4, ' ').nth(3).unwrap().to_string())
                .collect()
        };
        let all = || Filter {
            namespace: Some("meet".into()),
            since: Some(60),
            ..Default::default()
        };
        assert_eq!(
            texts(all()),
            ["Directory: reset", "checkpoint starting", "directory: ok"]
        );
        assert_eq!(
            texts(Filter {
                pod: Some("meet-".into()),
                matches: Some("DIRECTORY".into()),
                ..all()
            }),
            ["Directory: reset", "directory: ok"]
        );
        assert_eq!(
            texts(Filter {
                limit: Some(1),
                ..all()
            }),
            ["directory: ok"]
        );
        assert_eq!(
            texts(Filter {
                node: Some("hel1-2".into()),
                ..all()
            }),
            ["checkpoint starting"]
        );
        assert_eq!(
            texts(Filter {
                since: Some(12),
                ..all()
            }),
            ["checkpoint starting", "directory: ok"]
        );
        assert_eq!(
            texts(Filter {
                namespace: Some("other".into()),
                ..all()
            }),
            Vec::<String>::new()
        );
        let first = &query(&cfg, &all(), now)[0];
        assert_eq!(
            first,
            &format!(
                "{} hel1-1 meet-a/c Directory: reset",
                rfc3339((midnight - 5) * NANOS, 3)
            )
        );
        assert_eq!(newest(meet), Some((midnight + 5) * NANOS));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prunes_each_target_by_its_retention_then_all_by_size() {
        let root = dir("prune");
        let cfg = config(&root, "meet/meet-=3d,meet/glugg-db-=1d", 1 << 20);
        let today = T0_SECS / DAY * DAY;
        for back in 0..5 {
            append(
                &cfg.targets[0],
                &[line(today - back * DAY + 60, "n", "meet-a1234", "x")],
            )
            .unwrap();
            append(
                &cfg.targets[1],
                &[line(today - back * DAY + 60, "n", "glugg-db-1", "x")],
            )
            .unwrap();
        }
        prune(&cfg, T0_SECS);
        // 3 days back from T0 reaches into the day 3 back; 1 day into yesterday.
        assert_eq!(files(&cfg.targets[0]).len(), 4);
        assert_eq!(files(&cfg.targets[1]).len(), 2);
        // Now cap the total at three files' worth: the oldest days go, today's never.
        let size = files(&cfg.targets[0])[0].2;
        let capped = Config {
            max_bytes: 3 * size,
            ..config(&root, "meet/meet-=3d,meet/glugg-db-=1d", 0)
        };
        prune(&capped, T0_SECS);
        let days = |t: &Target| {
            files(t)
                .iter()
                .map(|(d, ..)| T0_SECS / DAY - d)
                .collect::<Vec<_>>()
        };
        assert_eq!(days(&capped.targets[0]), [1, 0]);
        assert_eq!(days(&capped.targets[1]), [0]);
        prune(
            &Config {
                max_bytes: 1,
                ..config(&root, "meet/meet-=3d,meet/glugg-db-=1d", 0)
            },
            T0_SECS,
        );
        assert_eq!(
            files(&capped.targets[0]).len() + files(&capped.targets[1]).len(),
            2,
            "today's stay"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_token_is_compared_whole() {
        assert!(same(b"secret", b"secret"));
        assert!(!same(b"secret", b"secreT"));
        assert!(!same(b"secret", b"secret!"));
        assert!(!same(b"", b"secret"));
    }
}
