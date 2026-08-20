//! kubeloupe -- the whole metrics backend for Lens in one process.
//!
//! Lens will not draw a chart without a Prometheus-compatible query API,
//! and the usual way to get one on a small cluster costs a Prometheus, a
//! node-exporter and a kube-state-metrics. All three exist to produce
//! about twenty series that the API server and the kubelet already know.
//! This reads them directly, keeps a day of samples in memory, and
//! answers the subset of PromQL Lens actually generates.

mod api;
mod chunk;
mod collect;
mod discovery;
mod kube;
mod promql;
mod snapshot;
mod store;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Config {
    /// 30s against Lens' 60s display step gives rate() two samples per
    /// point, which is the minimum it can work from. Halving it doubles
    /// the memory the store holds.
    scrape_interval: u64,
    retention: i64,
    listen: String,
    /// Unset disables persistence entirely, so the daemon still runs
    /// without a volume attached.
    snapshot_path: Option<PathBuf>,
    /// The upper bound on how much history an unclean kill can cost.
    snapshot_interval: i64,
}

impl Config {
    fn from_env() -> Self {
        Self {
            scrape_interval: env_or::<u64>("SCRAPE_INTERVAL_SECONDS", 30).max(1),
            retention: env_or::<i64>("RETENTION_HOURS", 24).max(1) * 3600,
            listen: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string()),
            snapshot_path: std::env::var("SNAPSHOT_PATH")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .map(PathBuf::from),
            snapshot_interval: env_or::<i64>("SNAPSHOT_INTERVAL_SECONDS", 300).max(1),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::from_env();
    let client = kube::Client::in_cluster()?;

    let store = restore(&config);
    let store: api::Shared = Arc::new(RwLock::new(store));

    let collector = tokio::spawn(collector_loop(
        Arc::clone(&store),
        client,
        config.scrape_interval,
        config.snapshot_path.clone(),
        config.snapshot_interval,
    ));

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    eprintln!(
        "kubeloupe: listening on {}, scraping every {}s, keeping {}h, snapshot {}",
        config.listen,
        config.scrape_interval,
        config.retention / 3600,
        match &config.snapshot_path {
            Some(path) => format!("every {}s to {}", config.snapshot_interval, path.display()),
            None => "disabled".to_string(),
        },
    );

    // Racing the signal against the server rather than using
    // with_graceful_shutdown: that waits for every connection to close,
    // and Lens holds keep-alive connections through the API server proxy.
    // Waiting for them could burn the whole termination grace period and
    // end in SIGKILL -- with the final snapshot never written, which is
    // precisely the case it exists for. Dropping a request mid-flight
    // costs Lens one retry, which it already has five of.
    let server = axum::serve(listener, api::router(Arc::clone(&store)));
    tokio::select! {
        result = server => result?,
        _ = shutdown_signal() => eprintln!("kubeloupe: shutting down"),
    }

    // Stopping the collector first means nothing is mid-write below.
    collector.abort();
    if let Some(path) = &config.snapshot_path {
        let guard = store.read().await;
        match snapshot::save(&guard, path) {
            Ok(()) => eprintln!("kubeloupe: final snapshot written to {}", path.display()),
            Err(error) => eprintln!("kubeloupe: final snapshot failed: {error:#}"),
        }
    }

    Ok(())
}

fn restore(config: &Config) -> store::Store {
    let Some(path) = &config.snapshot_path else {
        return store::Store::new(config.retention);
    };
    if !path.exists() {
        return store::Store::new(config.retention);
    }

    match snapshot::load(path, config.retention, now()) {
        Ok(store) => {
            eprintln!(
                "kubeloupe: restored {} series and {} samples from {}",
                store.series_count(),
                store.sample_count(),
                path.display(),
            );
            store
        }
        // Never fatal. A snapshot that cannot be read must cost history,
        // not availability -- a daemon that crash-loops on its own state
        // file is worse than one that starts empty.
        Err(error) => {
            eprintln!(
                "kubeloupe: ignoring unreadable snapshot {}: {error:#}",
                path.display()
            );
            store::Store::new(config.retention)
        }
    }
}

async fn collector_loop(
    store: api::Shared,
    client: kube::Client,
    scrape_interval: u64,
    snapshot_path: Option<PathBuf>,
    snapshot_interval: i64,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(scrape_interval));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_snapshot = now();

    loop {
        ticker.tick().await;
        let started = now();

        {
            // The write lock is held for one pass. Collection is a handful
            // of HTTP calls, so a query can block behind it briefly; two
            // stores to swap between would cost twice the memory to avoid
            // a wait Lens never notices.
            let mut guard = store.write().await;
            if let Err(error) = collect::collect(&client, &mut guard, started).await {
                eprintln!("kubeloupe: collection failed: {error:#}");
            }
            // Outside the error path on purpose. Pruning used to be the
            // last line of collect(), which meant an API server outage --
            // exactly when collection returns early -- also stopped
            // anything ageing out.
            guard.prune(started);
        }

        let Some(path) = &snapshot_path else { continue };
        if started - last_snapshot < snapshot_interval {
            continue;
        }
        // Taken under a READ lock, after the write guard above is dropped,
        // so collection and queries are not blocked by the file write.
        let guard = store.read().await;
        match snapshot::save(&guard, path) {
            Ok(()) => last_snapshot = started,
            // Keep serving from memory and try again next time: a full or
            // unwritable volume should not take the metrics down.
            Err(error) => eprintln!("kubeloupe: snapshot failed: {error:#}"),
        }
    }
}

/// SIGTERM as well as Ctrl-C: SIGTERM is what Kubernetes actually sends,
/// and handling only Ctrl-C would mean every ordinary rollout skipped the
/// final snapshot and lost the interval's history.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
