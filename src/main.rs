//! lens-metricsd -- the whole metrics backend for Lens in one process.
//!
//! Lens will not draw a chart without a Prometheus-compatible query API,
//! and the usual way to get one on a small cluster costs a Prometheus, a
//! node-exporter and a kube-state-metrics. All three exist to produce
//! about twenty series that the API server and the kubelet already know.
//! This reads them directly, keeps a day of samples in memory, and
//! answers the subset of PromQL Lens actually generates.

mod api;
mod collect;
mod kube;
mod promql;
mod store;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // 30s against Lens' 60s display step gives rate() two samples per
    // point, which is the minimum it can work from. Halving it doubles
    // the memory the store holds.
    let interval: u64 = env_or("SCRAPE_INTERVAL_SECONDS", 30);
    let retention_hours: i64 = env_or("RETENTION_HOURS", 24);
    let listen: String =
        std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());

    let client = kube::Client::in_cluster()?;
    let store: api::Shared = Arc::new(RwLock::new(store::Store::new(retention_hours * 3600)));

    let collector = {
        let store = Arc::clone(&store);
        async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval.max(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let started = now();
                // The write lock is taken for the duration of one pass.
                // Collection is a handful of HTTP calls, so a query can
                // block behind it briefly; two stores to swap between
                // would cost twice the memory to avoid a wait Lens never
                // notices.
                let mut guard = store.write().await;
                if let Err(error) = collect::collect(&client, &mut guard, started).await {
                    eprintln!("lens-metricsd: collection failed: {error:#}");
                }
            }
        }
    };
    tokio::spawn(collector);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    eprintln!(
        "lens-metricsd: listening on {listen}, scraping every {interval}s, keeping {retention_hours}h"
    );

    axum::serve(listener, api::router(store))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
