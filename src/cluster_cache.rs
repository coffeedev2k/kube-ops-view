//! Watch-backed cache of one Kubernetes cluster.
//!
//! - nodes and pods: one LIST (`resourceVersion=0`, served from the apiserver watch cache, not etcd),
//!   then a WATCH stream with only the changes (like client-go informers). The watcher resumes
//!   after errors and re-lists after `410 Gone`.
//! - metrics.k8s.io: polled every metrics interval (metrics-server resolution is 15 s by default).
//! - if WATCH is forbidden (old RBAC without verb "watch"): LIST every [`FALLBACK_LIST_INTERVAL`].
//!
//! The update loop builds the UI cluster data from this cache: no API requests per update.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ApiResource, ListParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::cluster_discovery::Cluster;
use crate::kubernetes::{
    CachedPod, LeanNode, LeanPod, MetricsList, NodeMetrics, PodKey, PodMetrics, build_cluster_data,
    map_node, map_pod,
};

/// A watch error newer than this marks the cluster query as failed (the watcher retries with backoff up to 30 s).
const WATCH_ERROR_WINDOW: Duration = Duration::from_secs(45);
/// Page size of the initial LIST (less memory per response than one big list).
const LIST_PAGE_SIZE: u32 = 500;
const FALLBACK_LIST_INTERVAL: Duration = Duration::from_secs(10);

/// The watch of nodes or pods failed recently; the cached data can be stale.
#[derive(Debug)]
pub struct WatchFailure(pub String);

impl std::fmt::Display for WatchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WatchFailure {}

#[derive(Default)]
pub struct ClusterCache {
    cluster_id: String,
    nodes: Mutex<HashMap<String, Value>>,
    pods: Mutex<HashMap<PodKey, CachedPod>>,
    node_usage: Mutex<HashMap<String, Value>>,
    pod_usage: Mutex<HashMap<PodKey, HashMap<String, Value>>>,
    nodes_ready: AtomicBool,
    pods_ready: AtomicBool,
    last_error: Mutex<Option<(String, Instant)>>,
}

impl ClusterCache {
    fn set_error(&self, message: String) {
        *self.last_error.lock().unwrap() = Some((message, Instant::now()));
    }

    fn clear_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }

    /// Cluster data for the UI; `None` while the initial list of nodes or pods is not complete.
    pub fn cluster_data(&self, cluster: &Cluster, now: f64) -> anyhow::Result<Option<Value>> {
        if let Some((message, at)) = self.last_error.lock().unwrap().as_ref()
            && at.elapsed() < WATCH_ERROR_WINDOW
        {
            return Err(WatchFailure(message.clone()).into());
        }
        if !self.nodes_ready.load(Ordering::SeqCst) || !self.pods_ready.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let nodes = self.nodes.lock().unwrap();
        let pods = self.pods.lock().unwrap();
        let node_usage = self.node_usage.lock().unwrap();
        let pod_usage = self.pod_usage.lock().unwrap();
        Ok(Some(build_cluster_data(
            cluster,
            &nodes,
            &pods,
            &node_usage,
            &pod_usage,
            now,
        )))
    }
}

/// Cache and its background tasks; dropping it (cluster removed) stops the watches.
pub struct ClusterWatches {
    pub cache: Arc<ClusterCache>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for ClusterWatches {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        debug!("Stopped watches of cluster {}", self.cache.cluster_id);
    }
}

pub fn start(cluster_id: &str, client: kube::Client, metrics_interval: Duration) -> ClusterWatches {
    info!(
        "Start watching nodes and pods of cluster {cluster_id}, metrics every {} s",
        metrics_interval.as_secs_f64()
    );
    let cache = Arc::new(ClusterCache {
        cluster_id: cluster_id.to_string(),
        ..Default::default()
    });
    let nodes: Api<LeanNode> = Api::all_with(client.clone(), &ApiResource::erase::<Node>(&()));
    let pods: Api<LeanPod> = Api::all_with(client.clone(), &ApiResource::erase::<Pod>(&()));
    let tasks = vec![
        tokio::spawn(sync(
            nodes,
            "nodes",
            cache.clone(),
            |c| &c.nodes,
            |c| &c.nodes_ready,
            map_node,
        )),
        tokio::spawn(sync(
            pods,
            "pods",
            cache.clone(),
            |c| &c.pods,
            |c| &c.pods_ready,
            map_pod,
        )),
        tokio::spawn(poll_metrics(client, cache.clone(), metrics_interval)),
    ];
    ClusterWatches { cache, tasks }
}

type Entries<Key, V> = fn(&ClusterCache) -> &Mutex<HashMap<Key, V>>;
type Ready = fn(&ClusterCache) -> &AtomicBool;

fn is_watch_forbidden(e: &watcher::Error) -> bool {
    match e {
        watcher::Error::WatchStartFailed(kube::Error::Api(s))
        | watcher::Error::WatchFailed(kube::Error::Api(s)) => s.code == 403,
        watcher::Error::WatchError(s) => s.code == 403,
        _ => false,
    }
}

fn is_gone(e: &watcher::Error) -> bool {
    match e {
        watcher::Error::WatchStartFailed(kube::Error::Api(s))
        | watcher::Error::WatchFailed(kube::Error::Api(s)) => s.code == 410,
        watcher::Error::WatchError(s) => s.code == 410,
        _ => false,
    }
}

/// Keep `entries` in sync with the API objects of one kind.
async fn sync<K, Key, V>(
    api: Api<K>,
    kind: &'static str,
    cache: Arc<ClusterCache>,
    entries: Entries<Key, V>,
    ready: Ready,
    to_entry: fn(&K) -> Option<(Key, V)>,
) where
    K: kube::Resource + Clone + DeserializeOwned + Debug + Send + 'static,
    Key: Eq + Hash,
{
    let config = watcher::Config::default()
        .any_semantic()
        .page_size(LIST_PAGE_SIZE);
    let mut stream = watcher::watcher(api.clone(), config)
        .default_backoff()
        .boxed();
    // objects of the current (re-)list; replace all entries at once when the list is complete
    let mut listing: Option<HashMap<Key, V>> = None;
    let id = &cache.cluster_id;
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Init) => listing = Some(HashMap::new()),
            Ok(Event::InitApply(obj)) => {
                if let (Some(listing), Some((key, value))) = (listing.as_mut(), to_entry(&obj)) {
                    listing.insert(key, value);
                }
            }
            Ok(Event::InitDone) => {
                if let Some(listing) = listing.take() {
                    info!(
                        "Listed {} {kind} of cluster {id}, watching changes",
                        listing.len()
                    );
                    *entries(&cache).lock().unwrap() = listing;
                }
                ready(&cache).store(true, Ordering::SeqCst);
                cache.clear_error();
            }
            Ok(Event::Apply(obj)) => {
                if let Some((key, value)) = to_entry(&obj) {
                    entries(&cache).lock().unwrap().insert(key, value);
                }
                cache.clear_error();
            }
            Ok(Event::Delete(obj)) => {
                if let Some((key, _)) = to_entry(&obj) {
                    entries(&cache).lock().unwrap().remove(&key);
                }
                cache.clear_error();
            }
            Err(e) if is_watch_forbidden(&e) => {
                warn!(
                    "Watch of {kind} in cluster {id} is forbidden: {e}. RBAC needs verb \"watch\" on {kind} \
                     (see deploy/rbac.yaml). Fall back to LIST every {} s.",
                    FALLBACK_LIST_INTERVAL.as_secs()
                );
                drop(stream);
                return poll_list(api, kind, cache, entries, ready, to_entry).await;
            }
            Err(e) if is_gone(&e) => {
                // normal: the watch history was compacted; the watcher lists again (no error state)
                info!("Watch of {kind} in cluster {id} expired (410 Gone), list again");
            }
            Err(e) => {
                warn!("Watch of {kind} in cluster {id} failed: {e} (retry with backoff)");
                cache.set_error(format!("watch {kind}: {e}"));
            }
        }
    }
}

/// Fallback without the "watch" permission: LIST (from the apiserver cache) every few seconds.
async fn poll_list<K, Key, V>(
    api: Api<K>,
    kind: &'static str,
    cache: Arc<ClusterCache>,
    entries: Entries<Key, V>,
    ready: Ready,
    to_entry: fn(&K) -> Option<(Key, V)>,
) where
    K: kube::Resource + Clone + DeserializeOwned + Debug + Send + 'static,
    Key: Eq + Hash,
{
    let params = ListParams::default().match_any();
    loop {
        match api.list(&params).await {
            Ok(list) => {
                *entries(&cache).lock().unwrap() = list.items.iter().filter_map(to_entry).collect();
                ready(&cache).store(true, Ordering::SeqCst);
                cache.clear_error();
            }
            Err(e) => {
                warn!("List of {kind} in cluster {} failed: {e}", cache.cluster_id);
                cache.set_error(format!("list {kind}: {e}"));
            }
        }
        tokio::time::sleep(FALLBACK_LIST_INTERVAL).await;
    }
}

async fn get_lean<T: DeserializeOwned>(
    client: &kube::Client,
    path: &str,
) -> Result<T, kube::Error> {
    let request = http::Request::get(path)
        .body(Vec::new())
        .map_err(kube::Error::HttpError)?;
    client.request::<T>(request).await
}

/// Poll node and pod metrics; without metrics (no metrics-server) the UI shows no usage bars.
async fn poll_metrics(client: kube::Client, cache: Arc<ClusterCache>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let id = &cache.cluster_id;
    let mut failing = false;
    loop {
        ticker.tick().await;
        let nodes =
            get_lean::<MetricsList<NodeMetrics>>(&client, "/apis/metrics.k8s.io/v1beta1/nodes")
                .await;
        let pods =
            get_lean::<MetricsList<PodMetrics>>(&client, "/apis/metrics.k8s.io/v1beta1/pods").await;
        let error = nodes
            .as_ref()
            .err()
            .or(pods.as_ref().err())
            .map(ToString::to_string);
        *cache.node_usage.lock().unwrap() = match nodes {
            Ok(list) => list
                .items
                .into_iter()
                .map(|m| (m.metadata.name, m.usage))
                .collect(),
            Err(_) => HashMap::new(),
        };
        *cache.pod_usage.lock().unwrap() = match pods {
            Ok(list) => list
                .items
                .into_iter()
                .map(|m| {
                    let containers = m
                        .containers
                        .into_iter()
                        .map(|c| (c.name, c.usage))
                        .collect();
                    ((m.metadata.namespace, m.metadata.name), containers)
                })
                .collect(),
            Err(_) => HashMap::new(),
        };
        match error {
            // log on state change only: without metrics-server this fails on every poll
            Some(e) if !failing => {
                warn!(
                    "Failed to query metrics for cluster {id}: {e} (no usage bars until metrics work)"
                );
                failing = true;
            }
            None if failing => {
                info!("Metrics of cluster {id} work again");
                failing = false;
            }
            _ => {}
        }
    }
}
