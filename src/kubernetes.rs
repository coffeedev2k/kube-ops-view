//! Query one Kubernetes cluster (nodes, pods, metrics) and map it to the UI cluster data.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::cluster_discovery::Cluster;
use crate::store::unix_now;

/// Finished (Succeeded/Failed) pods older than this are not shown.
const FINISHED_POD_MAX_AGE_SECONDS: f64 = 3600.0;

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
}

async fn get_json(client: &kube::Client, path: &str) -> anyhow::Result<Value> {
    let request = http::Request::get(path).body(Vec::new())?;
    Ok(client.request::<Value>(request).await?)
}

fn items(list: &Value) -> impl Iterator<Item = &Value> {
    list.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

/// Value at pointer; JSON null or a missing value gives the default.
fn or_default(v: &Value, pointer: &str, default: Value) -> Value {
    match v.pointer(pointer) {
        None | Some(Value::Null) => default,
        Some(x) => x.clone(),
    }
}

fn parse_time(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp() as f64 + f64::from(t.timestamp_subsec_micros()) / 1e6)
}

pub fn map_node(node: &Value) -> Value {
    json!({
        "name": str_at(node, "/metadata/name").unwrap_or(""),
        "labels": or_default(node, "/metadata/labels", json!({})),
        "status": {
            "addresses": or_default(node, "/status/addresses", json!([])),
            "capacity": or_default(node, "/status/capacity", json!({})),
            "allocatable": or_default(node, "/status/allocatable", json!({})),
        },
        "pods": {},
    })
}

pub fn map_pod(pod: &Value) -> Value {
    json!({
        "name": str_at(pod, "/metadata/name").unwrap_or(""),
        "namespace": str_at(pod, "/metadata/namespace").unwrap_or(""),
        "labels": or_default(pod, "/metadata/labels", json!({})),
        "phase": or_default(pod, "/status/phase", Value::Null),
        "startTime": or_default(pod, "/status/startTime", json!("")),
        "containers": [],
    })
}

/// Container spec fields, then all fields of the container status with the same name.
pub fn map_container(container: &Value, pod: &Value) -> Value {
    let name = container.get("name").cloned().unwrap_or(Value::Null);
    let mut obj = Map::new();
    obj.insert("name".into(), name.clone());
    obj.insert("image".into(), or_default(container, "/image", Value::Null));
    obj.insert(
        "resources".into(),
        or_default(container, "/resources", json!({})),
    );
    let status = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .and_then(|statuses| statuses.iter().find(|s| s.get("name") == Some(&name)));
    if let Some(Value::Object(status)) = status {
        for (k, v) in status {
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

/// A finished pod is hidden when its last container ended more than one hour ago or when it was evicted.
fn is_old_finished_pod(pod: &Value, mapped: &Value, now: f64) -> bool {
    let phase = mapped.get("phase").and_then(Value::as_str);
    if !matches!(phase, Some("Succeeded" | "Failed")) {
        return false;
    }
    let last_termination_time = mapped["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| str_at(c, "/state/terminated/finishedAt").and_then(parse_time))
        .fold(0.0, f64::max);
    (last_termination_time > 0.0 && last_termination_time < now - FINISHED_POD_MAX_AGE_SECONDS)
        || str_at(pod, "/status/reason") == Some("Evicted")
}

pub async fn query_kubernetes_cluster(cluster: &Cluster) -> anyhow::Result<Value> {
    let client = cluster
        .client
        .as_ref()
        .with_context(|| format!("cluster {} has no Kubernetes client", cluster.id))?;

    let node_list = get_json(client, "/api/v1/nodes").await?;
    let mut nodes: Map<String, Value> = Map::new();
    for node in items(&node_list) {
        let obj = map_node(node);
        let name = obj["name"].as_str().unwrap_or_default().to_string();
        nodes.insert(name, obj);
    }

    let now = unix_now();
    let pod_list = get_json(client, "/api/v1/pods").await?;
    // (namespace, name) -> (mapped pod, node name)
    let mut pods: HashMap<(String, String), (Value, Option<String>)> = HashMap::new();
    for pod in items(&pod_list) {
        let mut obj = map_pod(pod);
        if let Some(deleted) = str_at(pod, "/metadata/deletionTimestamp").and_then(parse_time) {
            obj["deleted"] = json!(deleted);
        }
        let containers: Vec<Value> = pod
            .pointer("/spec/containers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| map_container(c, pod))
            .collect();
        obj["containers"] = Value::Array(containers);
        if is_old_finished_pod(pod, &obj, now) {
            continue;
        }
        let key = (
            obj["namespace"].as_str().unwrap_or_default().to_string(),
            obj["name"].as_str().unwrap_or_default().to_string(),
        );
        let node_name = str_at(pod, "/spec/nodeName").map(str::to_string);
        pods.insert(key, (obj, node_name));
    }

    match get_json(client, "/apis/metrics.k8s.io/v1beta1/nodes").await {
        Ok(node_metrics_list) => {
            for node_metrics in items(&node_metrics_list) {
                let name = str_at(node_metrics, "/metadata/name").unwrap_or_default();
                if let Some(node) = nodes.get_mut(name) {
                    node["usage"] = or_default(node_metrics, "/usage", json!({}));
                }
            }
        }
        Err(e) => warn!(
            "Failed to query node metrics for cluster {}: {}",
            cluster.id,
            short_error_message(&e)
        ),
    }
    match get_json(client, "/apis/metrics.k8s.io/v1beta1/pods").await {
        Ok(pod_metrics_list) => {
            for pod_metrics in items(&pod_metrics_list) {
                let key = (
                    str_at(pod_metrics, "/metadata/namespace")
                        .unwrap_or_default()
                        .to_string(),
                    str_at(pod_metrics, "/metadata/name")
                        .unwrap_or_default()
                        .to_string(),
                );
                let Some((pod, _)) = pods.get_mut(&key) else {
                    continue;
                };
                let container_metrics: Vec<&Value> = pod_metrics
                    .get("containers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .collect();
                for container in pod["containers"].as_array_mut().into_iter().flatten() {
                    for metrics in &container_metrics {
                        if container.get("name") == metrics.get("name") {
                            container["resources"]["usage"] =
                                or_default(metrics, "/usage", json!({}));
                        }
                    }
                }
            }
        }
        Err(e) => warn!(
            "Failed to query pod metrics for cluster {}: {}",
            cluster.id,
            short_error_message(&e)
        ),
    }

    let mut unassigned_pods = Map::new();
    for ((namespace, name), (obj, node_name)) in pods {
        let pod_key = format!("{namespace}/{name}");
        match node_name.and_then(|n| nodes.get_mut(&n)) {
            Some(node) => {
                node["pods"][&pod_key] = obj;
            }
            None => {
                unassigned_pods.insert(pod_key, obj);
            }
        }
    }

    Ok(json!({
        "id": cluster.id,
        "api_server_url": cluster.api_server_url,
        "nodes": nodes,
        "unassigned_pods": unassigned_pods,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_status_overrides_spec() {
        let pod = json!({
            "status": {"containerStatuses": [
                {"name": "app", "image": "docker.io/foo:1", "ready": true, "restartCount": 2}
            ]}
        });
        let c = map_container(
            &json!({"name": "app", "image": "foo:1", "resources": {}}),
            &pod,
        );
        assert_eq!(c["image"], "docker.io/foo:1");
        assert_eq!(c["ready"], true);
        assert_eq!(c["restartCount"], 2);
    }

    #[test]
    fn old_finished_pod_is_hidden() {
        let now = 1_700_000_000.0;
        let finished = |at: &str| json!({"phase": "Succeeded", "containers": [{"state": {"terminated": {"finishedAt": at}}}]});
        let recent = chrono::DateTime::from_timestamp(now as i64 - 60, 0)
            .unwrap()
            .to_rfc3339();
        let old = chrono::DateTime::from_timestamp(now as i64 - 7200, 0)
            .unwrap()
            .to_rfc3339();
        assert!(!is_old_finished_pod(&json!({}), &finished(&recent), now));
        assert!(is_old_finished_pod(&json!({}), &finished(&old), now));
        let evicted = json!({"status": {"reason": "Evicted"}});
        assert!(is_old_finished_pod(
            &evicted,
            &json!({"phase": "Failed", "containers": []}),
            now
        ));
        assert!(!is_old_finished_pod(
            &evicted,
            &json!({"phase": "Running", "containers": []}),
            now
        ));
    }

    #[test]
    fn parse_kubernetes_time() {
        assert_eq!(parse_time("1970-01-01T00:01:00Z"), Some(60.0));
        assert_eq!(parse_time("garbage"), None);
    }
}
