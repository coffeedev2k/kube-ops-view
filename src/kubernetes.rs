//! Kubernetes objects -> UI cluster data.
//!
//! Lean API types keep only the fields the UI needs: serde skips all other fields
//! (managedFields, annotations, env, volumes, node images, ...) without allocating them.
//! The watch-backed [`ClusterCache`] keeps the mapped objects; [`query_kubernetes_cluster`]
//! builds the cluster data from the cache without API requests.

use std::collections::{BTreeMap, HashMap};

use kube::core::Object;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::cluster_cache::ClusterCache;
use crate::cluster_discovery::Cluster;
use crate::store::unix_now;

/// Finished (Succeeded/Failed) pods older than this are not shown.
const FINISHED_POD_MAX_AGE_SECONDS: f64 = 3600.0;

/// Container status fields for the UI (app/src/pod.js uses state, ready, restartCount).
/// Other fields (containerID, imageID, lastState, volumeMounts, ...) only cost memory and bandwidth.
const CONTAINER_STATUS_FIELDS: [&str; 4] = ["image", "ready", "restartCount", "state"];

/// Short reason why the cluster query failed, for example "401 Unauthorized".
pub fn short_error_message(e: &anyhow::Error) -> String {
    match e.downcast_ref::<kube::Error>() {
        Some(kube::Error::Api(status)) => {
            let code = status.code;
            let reason = http::StatusCode::from_u16(code)
                .ok()
                .and_then(|c| c.canonical_reason())
                .unwrap_or("");
            format!("{code} {reason}").trim().to_string()
        }
        Some(kube_error) => kube_error.to_string(),
        None => format!("{e:#}"),
    }
}

/// True for errors of the HTTP request (connection, TLS, HTTP status); false for internal errors.
pub fn is_request_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<kube::Error>().is_some()
        || e.downcast_ref::<crate::cluster_cache::WatchFailure>()
            .is_some()
}

// ---------- lean API types ----------

#[derive(Deserialize, Clone, Debug, Default)]
pub struct PodSpecLean {
    #[serde(rename = "nodeName")]
    pub node_name: Option<String>,
    #[serde(default)]
    pub containers: Vec<ContainerLean>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ContainerLean {
    pub name: String,
    pub image: Option<String>,
    pub resources: Option<Value>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct PodStatusLean {
    pub phase: Option<String>,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    pub reason: Option<String>,
    /// The [`CONTAINER_STATUS_FIELDS`] of each container status are merged into the UI container.
    #[serde(default, rename = "containerStatuses")]
    pub container_statuses: Vec<Value>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct NodeSpecLean {}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct NodeStatusLean {
    pub addresses: Option<Value>,
    pub capacity: Option<Value>,
    pub allocatable: Option<Value>,
}

pub type LeanPod = Object<PodSpecLean, PodStatusLean>;
pub type LeanNode = Object<NodeSpecLean, NodeStatusLean>;

/// `{"items": [...]}` of metrics.k8s.io lists.
#[derive(Deserialize)]
pub struct MetricsList<T> {
    pub items: Vec<T>,
}

#[derive(Deserialize)]
pub struct MetricsMeta {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
}

#[derive(Deserialize)]
pub struct NodeMetrics {
    pub metadata: MetricsMeta,
    pub usage: Value,
}

#[derive(Deserialize)]
pub struct PodMetrics {
    pub metadata: MetricsMeta,
    #[serde(default)]
    pub containers: Vec<ContainerMetrics>,
}

#[derive(Deserialize)]
pub struct ContainerMetrics {
    pub name: String,
    pub usage: Value,
}

// ---------- mapped objects (UI format) ----------

pub type PodKey = (String, String);

/// One pod in UI format (without metrics usage) and the data to place and filter it.
#[derive(Clone, Debug)]
pub struct CachedPod {
    pub node_name: Option<String>,
    pub evicted: bool,
    pub value: Value,
}

fn labels_value(labels: &Option<BTreeMap<String, String>>) -> Value {
    labels.as_ref().map_or_else(|| json!({}), |l| json!(l))
}

pub fn map_node(node: &LeanNode) -> Option<(String, Value)> {
    let name = node.metadata.name.clone()?;
    let status = node.status.clone().unwrap_or_default();
    let value = json!({
        "name": name,
        "labels": labels_value(&node.metadata.labels),
        "status": {
            "addresses": status.addresses.filter(|v| !v.is_null()).unwrap_or_else(|| json!([])),
            "capacity": status.capacity.filter(|v| !v.is_null()).unwrap_or_else(|| json!({})),
            "allocatable": status.allocatable.filter(|v| !v.is_null()).unwrap_or_else(|| json!({})),
        },
        "pods": {},
    });
    Some((name, value))
}

pub fn map_pod(pod: &LeanPod) -> Option<(PodKey, CachedPod)> {
    let meta = &pod.metadata;
    let name = meta.name.clone()?;
    let namespace = meta.namespace.clone().unwrap_or_default();
    let status = pod.status.clone().unwrap_or_default();
    let containers: Vec<Value> = pod
        .spec
        .containers
        .iter()
        .map(|c| {
            // container spec fields, then the UI fields of the container status with the same name
            let mut obj = Map::new();
            obj.insert("name".into(), json!(c.name));
            obj.insert("image".into(), json!(c.image));
            obj.insert(
                "resources".into(),
                c.resources
                    .clone()
                    .filter(|v| !v.is_null())
                    .unwrap_or_else(|| json!({})),
            );
            let container_status = status
                .container_statuses
                .iter()
                .find(|s| s.get("name").and_then(Value::as_str) == Some(c.name.as_str()));
            if let Some(Value::Object(fields)) = container_status {
                for field in CONTAINER_STATUS_FIELDS {
                    if let Some(v) = fields.get(field) {
                        obj.insert(field.to_string(), v.clone());
                    }
                }
            }
            Value::Object(obj)
        })
        .collect();
    let mut value = json!({
        "name": name,
        "namespace": namespace,
        "labels": labels_value(&meta.labels),
        "phase": status.phase,
        "startTime": status.start_time.unwrap_or_default(),
        "containers": containers,
    });
    if let Some(deleted) = &meta.deletion_timestamp {
        value["deleted"] = json!(deleted.0.as_second() as f64);
    }
    let cached = CachedPod {
        node_name: pod.spec.node_name.clone(),
        evicted: status.reason.as_deref() == Some("Evicted"),
        value,
    };
    Some(((namespace, name), cached))
}

fn parse_time(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp() as f64 + f64::from(t.timestamp_subsec_micros()) / 1e6)
}

/// A finished pod is hidden when its last container ended more than one hour ago or when it was evicted.
fn is_old_finished_pod(pod: &CachedPod, now: f64) -> bool {
    let phase = pod.value.get("phase").and_then(Value::as_str);
    if !matches!(phase, Some("Succeeded" | "Failed")) {
        return false;
    }
    let last_termination_time = pod.value["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            c.pointer("/state/terminated/finishedAt")
                .and_then(Value::as_str)
                .and_then(parse_time)
        })
        .fold(0.0, f64::max);
    (last_termination_time > 0.0 && last_termination_time < now - FINISHED_POD_MAX_AGE_SECONDS)
        || pod.evicted
}

/// Cluster data for the UI from cached nodes, pods, and metrics.
pub fn build_cluster_data(
    cluster: &Cluster,
    nodes: &HashMap<String, Value>,
    pods: &HashMap<PodKey, CachedPod>,
    node_usage: &HashMap<String, Value>,
    pod_usage: &HashMap<PodKey, HashMap<String, Value>>,
    now: f64,
) -> Value {
    let mut node_values: Map<String, Value> = nodes
        .iter()
        .map(|(name, node)| {
            let mut node = node.clone();
            if let Some(usage) = node_usage.get(name) {
                node["usage"] = usage.clone();
            }
            (name.clone(), node)
        })
        .collect();
    let mut unassigned_pods = Map::new();
    for (key, pod) in pods {
        if is_old_finished_pod(pod, now) {
            continue;
        }
        let mut value = pod.value.clone();
        if let Some(container_usage) = pod_usage.get(key) {
            for container in value["containers"].as_array_mut().into_iter().flatten() {
                let usage = container
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|name| container_usage.get(name));
                if let Some(usage) = usage {
                    container["resources"]["usage"] = usage.clone();
                }
            }
        }
        let pod_key = format!("{}/{}", key.0, key.1);
        match pod.node_name.as_ref().and_then(|n| node_values.get_mut(n)) {
            Some(node) => node["pods"][&pod_key] = value,
            None => {
                unassigned_pods.insert(pod_key, value);
            }
        }
    }
    json!({
        "id": cluster.id,
        "api_server_url": cluster.api_server_url,
        "nodes": node_values,
        "unassigned_pods": unassigned_pods,
    })
}

/// Cluster data from the watch-backed cache; `None` while the initial list is not complete.
pub async fn query_kubernetes_cluster(
    cluster: &Cluster,
    metrics_interval: std::time::Duration,
) -> anyhow::Result<Option<Value>> {
    let cache: &ClusterCache = cluster.cache(metrics_interval)?;
    cache.cluster_data(cluster, unix_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lean_pod(json: Value) -> LeanPod {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn pod_mapping_keeps_ui_fields_only() {
        let pod = lean_pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "name": "web-1", "namespace": "default", "labels": {"app": "web"},
                "annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{...}"},
                "managedFields": [{"manager": "kubelet", "fieldsV1": {"f:status": {}}}],
                "deletionTimestamp": "1970-01-01T00:01:00Z"
            },
            "spec": {
                "nodeName": "node-a",
                "containers": [{"name": "app", "image": "nginx:1", "env": [{"name": "A", "value": "B"}],
                                "resources": {"requests": {"cpu": "100m"}}}],
                "volumes": [{"name": "v"}]
            },
            "status": {
                "phase": "Running", "startTime": "2026-01-01T00:00:00Z", "podIP": "10.0.0.1",
                "conditions": [{"type": "Ready"}],
                "containerStatuses": [{"name": "app", "image": "docker.io/library/nginx:1", "ready": true, "restartCount": 2,
                                       "state": {"running": {}}, "containerID": "containerd://abc", "imageID": "sha256:def",
                                       "lastState": {"terminated": {"message": "long text"}}, "started": true}]
            }
        }));
        let (key, cached) = map_pod(&pod).unwrap();
        assert_eq!(key, ("default".to_string(), "web-1".to_string()));
        assert_eq!(cached.node_name.as_deref(), Some("node-a"));
        assert_eq!(
            cached.value,
            json!({
                "name": "web-1", "namespace": "default", "labels": {"app": "web"},
                "phase": "Running", "startTime": "2026-01-01T00:00:00Z", "deleted": 60.0,
                "containers": [{"name": "app", "image": "docker.io/library/nginx:1",
                                "resources": {"requests": {"cpu": "100m"}},
                                "ready": true, "restartCount": 2, "state": {"running": {}}}]
            })
        );
    }

    #[test]
    fn pod_without_status_and_labels() {
        let pod = lean_pod(
            json!({"metadata": {"name": "p", "namespace": "ns"}, "spec": {"containers": [{"name": "c"}]}}),
        );
        let (_, cached) = map_pod(&pod).unwrap();
        assert_eq!(cached.node_name, None);
        assert_eq!(
            cached.value,
            json!({"name": "p", "namespace": "ns", "labels": {}, "phase": null, "startTime": "",
                   "containers": [{"name": "c", "image": null, "resources": {}}]})
        );
    }

    #[test]
    fn node_mapping_skips_images_and_conditions() {
        let node: LeanNode = serde_json::from_value(json!({
            "metadata": {"name": "node-a", "labels": {"zone": "a"}},
            "spec": {"providerID": "aws:///i-1"},
            "status": {"capacity": {"cpu": "4"}, "allocatable": {"cpu": "3900m"}, "addresses": null,
                       "images": [{"names": ["x"], "sizeBytes": 1}], "conditions": [{"type": "Ready"}]}
        }))
        .unwrap();
        let (name, value) = map_node(&node).unwrap();
        assert_eq!(name, "node-a");
        assert_eq!(
            value,
            json!({"name": "node-a", "labels": {"zone": "a"},
                   "status": {"addresses": [], "capacity": {"cpu": "4"}, "allocatable": {"cpu": "3900m"}},
                   "pods": {}})
        );
    }

    #[test]
    fn build_places_pods_and_metrics() {
        let cluster = Cluster::new("c1".into(), "c1".into(), "https://c1".into(), None);
        let nodes: HashMap<String, Value> = [map_node(
            &serde_json::from_value(json!({"metadata": {"name": "node-a"}, "spec": {}})).unwrap(),
        )
        .unwrap()]
        .into();
        let on_node = map_pod(&lean_pod(
            json!({"metadata": {"name": "a", "namespace": "ns"},
            "spec": {"nodeName": "node-a", "containers": [{"name": "c"}]}}),
        ))
        .unwrap();
        let pending = map_pod(&lean_pod(
            json!({"metadata": {"name": "b", "namespace": "ns"},
            "spec": {"containers": [{"name": "c"}]}, "status": {"phase": "Pending"}}),
        ))
        .unwrap();
        let evicted = map_pod(&lean_pod(json!({"metadata": {"name": "e", "namespace": "ns"},
            "spec": {"nodeName": "node-a", "containers": []}, "status": {"phase": "Failed", "reason": "Evicted"}}))).unwrap();
        let pods: HashMap<PodKey, CachedPod> = [on_node, pending, evicted].into();
        let node_usage: HashMap<String, Value> =
            [("node-a".to_string(), json!({"cpu": "1"}))].into();
        let pod_usage: HashMap<PodKey, HashMap<String, Value>> = [(
            ("ns".to_string(), "a".to_string()),
            [("c".to_string(), json!({"cpu": "5m"}))].into(),
        )]
        .into();
        let data = build_cluster_data(&cluster, &nodes, &pods, &node_usage, &pod_usage, 1e9);
        assert_eq!(data["nodes"]["node-a"]["usage"], json!({"cpu": "1"}));
        assert_eq!(
            data["nodes"]["node-a"]["pods"]["ns/a"]["containers"][0]["resources"]["usage"],
            json!({"cpu": "5m"})
        );
        assert!(
            data["nodes"]["node-a"]["pods"].get("ns/e").is_none(),
            "evicted pod is hidden"
        );
        assert_eq!(data["unassigned_pods"]["ns/b"]["phase"], "Pending");
    }

    #[test]
    fn old_finished_pod_is_hidden() {
        let now = 1_700_000_000.0;
        let finished = |at: &str| CachedPod {
            node_name: None,
            evicted: false,
            value: json!({"phase": "Succeeded", "containers": [{"state": {"terminated": {"finishedAt": at}}}]}),
        };
        let recent = chrono::DateTime::from_timestamp(now as i64 - 60, 0)
            .unwrap()
            .to_rfc3339();
        let old = chrono::DateTime::from_timestamp(now as i64 - 7200, 0)
            .unwrap()
            .to_rfc3339();
        assert!(!is_old_finished_pod(&finished(&recent), now));
        assert!(is_old_finished_pod(&finished(&old), now));
    }
}
