//! Update loop: query all discovered clusters, store the cluster data, publish UI events.
//!
//! Events for the UI:
//! - `clusterstatus` `{cluster_id, status}`: before the first cluster data, and after a failed query
//! - `clusterupdate` `<cluster data>`: first data of a newly discovered cluster
//! - `clusterdelta` `{cluster_id, delta}`: JSON-delta to the last cluster data, after each successful query

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::cluster_discovery::{Cluster, ClusterDiscoverer};
use crate::kubernetes::{is_request_error, query_kubernetes_cluster, short_error_message};
use crate::mock::query_mock_cluster;
use crate::store::{Backoff, Store, unix_now};

const BACKOFF_FACTOR_SECONDS: f64 = 2.0;
const BACKOFF_MAX_SECONDS: f64 = 60.0;
const BACKOFF_JITTER_SECONDS: f64 = 4.0;
const LOOP_SLEEP_SECONDS: f64 = 2.0;
const LOOP_SLEEP_JITTER_SECONDS: f64 = 1.0;

#[derive(Clone, Copy)]
pub enum ClusterQuery {
    /// Cluster data from the watch-backed cache; metrics are polled every `metrics_interval`.
    Kubernetes {
        metrics_interval: Duration,
    },
    Mock,
}

impl ClusterQuery {
    /// `None`: the cluster has no data yet (initial list in progress), not an error.
    async fn query(self, cluster: &Cluster) -> anyhow::Result<Option<serde_json::Value>> {
        match self {
            ClusterQuery::Kubernetes { metrics_interval } => {
                query_kubernetes_cluster(cluster, metrics_interval).await
            }
            ClusterQuery::Mock => query_mock_cluster(cluster).map(Some),
        }
    }
}

fn random_jitter(value: f64, jitter: f64) -> f64 {
    value + rand::rng().random_range(0.0..=jitter)
}

/// Exponential backoff: factor * 2^tries, max 60 seconds, plus up to 4 seconds of jitter.
pub fn calculate_backoff(tries: u32) -> f64 {
    let exp = BACKOFF_FACTOR_SECONDS * 2f64.powi(tries.min(30) as i32);
    random_jitter(exp.min(BACKOFF_MAX_SECONDS), BACKOFF_JITTER_SECONDS)
}

fn handle_query_failure(e: &anyhow::Error, cluster: &Cluster, backoff: Option<Backoff>) -> Backoff {
    let tries = backoff.map_or(0, |b| b.tries) + 1;
    let wait_seconds = calculate_backoff(tries);
    let message = if is_request_error(e) {
        short_error_message(e)
    } else {
        format!("{e:#}")
    };
    error!(
        "Failed to query cluster {} ({}): {} (try {}, wait {} seconds)",
        cluster.id,
        cluster.api_server_url,
        message,
        tries,
        wait_seconds.round()
    );
    Backoff {
        tries,
        next_try: unix_now() + wait_seconds,
    }
}

pub async fn update_clusters(
    discoverer: Arc<ClusterDiscoverer>,
    cluster_query: ClusterQuery,
    store: Arc<Store>,
    query_interval: f64,
) {
    loop {
        match store.acquire_lock().await {
            Ok(Some(lock)) => {
                update_all(&discoverer, cluster_query, &store, query_interval).await;
                if let Err(e) = store.release_lock(lock).await {
                    warn!("Failed to release update lock: {e:#}");
                }
            }
            // another replica holds the lock
            Ok(None) => debug!("Update lock is taken, skip this round"),
            Err(e) => error!("Failed to acquire update lock: {e:#}"),
        }
        let sleep =
            random_jitter(LOOP_SLEEP_SECONDS, LOOP_SLEEP_JITTER_SECONDS).min(query_interval);
        tokio::time::sleep(Duration::from_secs_f64(sleep.max(0.1))).await;
    }
}

async fn update_all(
    discoverer: &ClusterDiscoverer,
    cluster_query: ClusterQuery,
    store: &Store,
    query_interval: f64,
) {
    let clusters = discoverer.get_clusters().await;
    let updates = clusters
        .iter()
        .map(|cluster| update_cluster(cluster, cluster_query, store, query_interval));
    for (cluster, result) in clusters
        .iter()
        .zip(futures::future::join_all(updates).await)
    {
        if let Err(e) = result {
            error!("Failed to update cluster {}: {e:#}", cluster.id);
        }
    }
    let cluster_ids: BTreeSet<String> = clusters.iter().map(|c| c.id.clone()).collect();
    if let Err(e) = store.set_cluster_ids(&cluster_ids).await {
        error!("Failed to store cluster IDs: {e:#}");
    }
}

async fn update_cluster(
    cluster: &Cluster,
    cluster_query: ClusterQuery,
    store: &Store,
    query_interval: f64,
) -> anyhow::Result<()> {
    let mut status = store.get_cluster_status(&cluster.id).await?;
    let now = unix_now();
    if now < status.last_query_time.unwrap_or(0.0) + query_interval {
        return Ok(());
    }
    if status.backoff.as_ref().is_some_and(|b| now < b.next_try) {
        // cluster is still in backoff, skip
        return Ok(());
    }

    debug!(
        "Querying cluster {} ({})..",
        cluster.id, cluster.api_server_url
    );
    match cluster_query.query(cluster).await {
        Err(e) => {
            status.backoff = Some(handle_query_failure(&e, cluster, status.backoff.take()));
            store
                .publish(
                    "clusterstatus",
                    &json!({"cluster_id": cluster.id, "status": status}),
                )
                .await?;
        }
        Ok(None) => {
            debug!(
                "Cluster {} has no data yet (initial list in progress)",
                cluster.id
            );
            return Ok(());
        }
        Ok(Some(data)) => {
            status.last_query_time = Some(now);
            if let Some(backoff) = status.backoff.take() {
                info!(
                    "Cluster {} ({}) recovered after {} tries.",
                    cluster.id, cluster.api_server_url, backoff.tries
                );
            }
            let data = Arc::new(data);
            match store.get_cluster_data(&cluster.id).await? {
                Some(old_data) => {
                    let delta = crate::json_delta::diff(&old_data, &data);
                    // an empty delta also tells the UI that the query succeeded
                    store
                        .publish(
                            "clusterdelta",
                            &json!({"cluster_id": cluster.id, "delta": delta}),
                        )
                        .await?;
                    if !delta.is_empty() {
                        store.set_cluster_data(&cluster.id, data).await?;
                    }
                }
                None => {
                    info!(
                        "Discovered new cluster {} ({}).",
                        cluster.id, cluster.api_server_url
                    );
                    // first send status with last_query_time!
                    store
                        .publish(
                            "clusterstatus",
                            &json!({"cluster_id": cluster.id, "status": status}),
                        )
                        .await?;
                    store.publish("clusterupdate", &data).await?;
                    store.set_cluster_data(&cluster.id, data).await?;
                }
            }
        }
    }
    store.set_cluster_status(&cluster.id, &status).await
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::store::StoreMessage;

    /// One update round for the mock clusters (query interval 0: always query).
    pub async fn update_mock_clusters(store: &Store) {
        update_all(&ClusterDiscoverer::mock(), ClusterQuery::Mock, store, 0.0).await;
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let b1 = calculate_backoff(1);
        assert!((4.0..=8.0).contains(&b1), "{b1}");
        let b10 = calculate_backoff(10);
        assert!((60.0..=64.0).contains(&b10), "{b10}");
        let b_huge = calculate_backoff(u32::MAX);
        assert!((60.0..=64.0).contains(&b_huge), "{b_huge}");
    }

    #[tokio::test]
    async fn first_query_publishes_status_then_update_then_delta() {
        let store = Store::memory();
        let mut rx = store.subscribe();
        let discoverer = ClusterDiscoverer::mock();
        update_all(&discoverer, ClusterQuery::Mock, &store, 0.0).await;

        let mut types = Vec::new();
        while let Ok(StoreMessage::Event(e)) = rx.try_recv() {
            types.push((e.event_type.clone(), e.cluster_id.clone().unwrap()));
        }
        assert_eq!(types.len(), 6);
        assert_eq!(types[0].0, "clusterstatus");
        assert_eq!(types[1], ("clusterupdate".to_string(), types[0].1.clone()));
        assert_eq!(store.get_cluster_ids().await.unwrap().len(), 3);

        update_all(&discoverer, ClusterQuery::Mock, &store, 0.0).await;
        let mut deltas = 0;
        while let Ok(StoreMessage::Event(e)) = rx.try_recv() {
            assert_eq!(e.event_type, "clusterdelta");
            deltas += 1;
        }
        assert_eq!(deltas, 3);
    }
}
