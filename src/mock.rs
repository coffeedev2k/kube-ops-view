//! Mock cluster data for the `--mock` mode (UI development and demos).
//!
//! The node/pod layout is deterministic (hash of indexes); resource values are random.
//! Every 13 seconds one worker node appears/disappears, every 7 seconds some pods appear/disappear.

use rand::Rng;
use rand::distr::Alphabetic;
use serde_json::{Map, Value, json};

use crate::cluster_discovery::Cluster;
use crate::store::unix_now;

const POD_NAMES: [&str; 8] = [
    "agent-cooper",
    "black-lodge",
    "bob",
    "bobby-briggs",
    "laura-palmer",
    "leland-palmer",
    "log-lady",
    "sheriff-truman",
];
const POD_LABELS: [(&str, [&str; 2]); 2] =
    [("env", ["prod", "dev"]), ("owner", ["x-wing", "iris"])];
const POD_PHASES: [&str; 4] = ["Pending", "Running", "Running", "Failed"];
const AVAILABILITY_ZONES: [&str; 3] = ["us-east-1a", "us-east-1b", "us-east-1c"];

/// Integer hash; u128 holds all intermediate values for the small mock inputs without overflow.
pub fn hash_int(x: u128) -> u128 {
    let x = ((x >> 16) ^ x) * 0x45D9F3B;
    let x = ((x >> 16) ^ x) * 0x45D9F3B;
    (x >> 16) ^ x
}

fn pick<T: Copy>(items: &[T], hash: u128) -> T {
    items[(hash % items.len() as u128) as usize]
}

fn time_slot_is_even(period_seconds: f64) -> bool {
    ((unix_now() / period_seconds) as u64).is_multiple_of(2)
}

fn generate_mock_pod(index: u128, i: u128, j: u128) -> Value {
    let mut rng = rand::rng();
    let mut labels = Map::new();
    for (li, (key, values)) in POD_LABELS.iter().enumerate() {
        let choice = hash_int((index + 1) * (i + 1) * (j + 1) * (li as u128 + 1))
            % (values.len() as u128 + 1);
        if choice != 0 {
            labels.insert(key.to_string(), json!(values[choice as usize - 1]));
        }
    }
    let phase = pick(&POD_PHASES, hash_int((index + 1) * (i + 1) * (j + 1)));

    let mut containers = Vec::new();
    for _ in 0..(1 + j % 2) {
        // generate "more real data"
        let requests_cpu: i64 = rng.random_range(10..=50);
        let requests_memory: i64 = rng.random_range(64..=256);
        // with max, we defend ourselves against negative cpu/memory
        let usage_cpu = (requests_cpu + rng.random_range(-30..=30)).max(1);
        let usage_memory = (requests_memory + rng.random_range(-64..=128)).max(1);
        let mut container = json!({
            "name": "myapp",
            "image": format!("foo/bar/{j}"),
            "resources": {
                "requests": {"cpu": format!("{requests_cpu}m"), "memory": format!("{requests_memory}Mi")},
                "limits": {},
                "usage": {"cpu": format!("{usage_cpu}m"), "memory": format!("{usage_memory}Mi")},
            },
            "ready": true,
            "state": {"running": {}},
        });
        match phase {
            "Running" if j.is_multiple_of(13) => {
                container["ready"] = json!(false);
                container["state"] = json!({"waiting": {"reason": "CrashLoopBackOff"}});
            }
            "Running" if j.is_multiple_of(7) => {
                container["ready"] = json!(false);
                container["state"] = json!({"running": {}});
                container["restartCount"] = json!(3);
            }
            "Failed" => {
                let obj = container.as_object_mut().expect("container is an object");
                obj.remove("state");
                obj.remove("ready");
            }
            _ => {}
        }
        containers.push(container);
    }

    let mut pod = json!({
        "name": format!("{}-{i}-{j}", pick(&POD_NAMES, hash_int((i + 1) * (j + 1)))),
        "namespace": if j < 3 { "kube-system" } else { "default" },
        "labels": labels,
        "phase": phase,
        "containers": containers,
    });
    if phase == "Running" && j.is_multiple_of(17) {
        pod["deleted"] = json!(123);
    }
    pod
}

fn pod_key(pod: &Value) -> String {
    format!(
        "{}/{}",
        pod["namespace"].as_str().unwrap_or_default(),
        pod["name"].as_str().unwrap_or_default()
    )
}

/// Parse "123m" / "64Mi" values that this module generates.
fn quantity_value(quantity: &Value, suffix: &str) -> i64 {
    quantity
        .as_str()
        .and_then(|s| s.strip_suffix(suffix))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub fn query_mock_cluster(cluster: &Cluster) -> anyhow::Result<Value> {
    let index: u128 = cluster
        .id
        .rsplit('-')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("mock cluster ID {} does not end with a number", cluster.id)
        })?;

    // Node layout per pool:
    //   Masters: 3 nodes, 1 per AZ
    //   Infra:   3-5 nodes, at least 1 per AZ (more for cluster index 1+)
    //   Workers: 3-7 nodes per AZ
    let mut infra_per_az = [1, 1, 1];
    if index >= 1 {
        infra_per_az[0] = 2;
    }
    if index >= 2 {
        infra_per_az[1] = 2;
    }
    let worker_per_az = [101, 202, 303].map(|seed| 3 + hash_int((index + 1) * seed) % 5);

    let mut node_specs: Vec<(&str, usize)> = Vec::new();
    for az in 0..3 {
        node_specs.push(("master", az));
    }
    for (az, count) in infra_per_az.iter().enumerate() {
        node_specs.extend(std::iter::repeat_n(("infra", az), *count));
    }
    for (az, count) in worker_per_az.iter().enumerate() {
        node_specs.extend(std::iter::repeat_n(("worker", az), *count as usize));
    }

    let mut rng = rand::rng();
    let mut nodes = Map::new();
    for (i, (role, az)) in node_specs.iter().enumerate() {
        // add/remove one worker node every 13 seconds for dynamism
        if *role == "worker" && i == node_specs.len() - 1 && time_slot_is_even(13.0) {
            continue;
        }
        let i = i as u128;

        let mut labels = Map::new();
        labels.insert(
            "topology.kubernetes.io/zone".into(),
            json!(AVAILABILITY_ZONES[*az]),
        );
        let role_labels: &[(&str, &str)] = match (*role, index) {
            ("master", 0) => &[
                ("node-role.kubernetes.io/master", ""),
                ("node-role.kubernetes.io/control-plane", ""),
            ],
            ("master", 1) => &[("node-role.kubernetes.io/control-plane", "")],
            ("master", _) => &[("kubernetes.io/role", "master")],
            ("infra", _) => &[("node-role.kubernetes.io/infra", "")],
            _ => &[("node-role.kubernetes.io/worker", "")],
        };
        for (k, v) in role_labels {
            labels.insert(k.to_string(), json!(v));
        }

        let mut pods = Map::new();
        for j in 0..hash_int((index + 1) * (i + 1)) % 32 {
            // add/remove some pods every 7 seconds
            if j % 17 == 0 && time_slot_is_even(7.0) {
                continue;
            }
            let pod = generate_mock_pod(index, i, j);
            pods.insert(pod_key(&pod), pod);
        }

        // node usage is the sum of the container usage
        let (mut usage_cpu, mut usage_memory) = (0, 0);
        for pod in pods.values() {
            for c in pod["containers"].as_array().into_iter().flatten() {
                usage_cpu += quantity_value(&c["resources"]["usage"]["cpu"], "m");
                usage_memory += quantity_value(&c["resources"]["usage"]["memory"], "Mi");
            }
        }

        // generate longer name for a node
        let suffix_len = rng.random_range(1..=20);
        let suffix: String = (&mut rng)
            .sample_iter(&Alphabetic)
            .take(suffix_len)
            .map(char::from)
            .collect();
        let name = format!("node-{i}-{suffix}");
        nodes.insert(
            name.clone(),
            json!({
                "name": name,
                "labels": labels,
                "status": {
                    "capacity": {"cpu": "8", "memory": "64Gi", "pods": "110"},
                    "allocatable": {"cpu": "7800m", "memory": "62Gi"},
                },
                "pods": pods,
                "usage": {"cpu": format!("{usage_cpu}m"), "memory": format!("{usage_memory}Mi")},
            }),
        );
    }

    let pod = generate_mock_pod(index, 11, index);
    Ok(json!({
        "id": format!("mock-cluster-{index}"),
        "api_server_url": format!("https://kube-{index}.example.org"),
        "nodes": nodes,
        "unassigned_pods": {pod_key(&pod): pod},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_discovery::ClusterDiscoverer;

    #[test]
    fn hash_int_matches_python() {
        // values from the Python implementation (arbitrary-precision int)
        assert_eq!(hash_int(1), 5364745905576871);
        assert_eq!(hash_int(101), 541836091371683456);
        assert_eq!(hash_int(909), 4876427266991366024);
        assert_eq!(hash_int(5760), 30900617205404190863);
    }

    #[tokio::test]
    async fn query_mock_clusters() {
        for cluster in ClusterDiscoverer::mock().get_clusters().await {
            let data = query_mock_cluster(&cluster).unwrap();
            assert!(data["id"].as_str().unwrap().starts_with("mock-cluster-"));
            let nodes = data["nodes"].as_object().unwrap();
            assert!(nodes.len() >= 3 + 3 + 9 - 1);
            assert_eq!(data["unassigned_pods"].as_object().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn mock_delta_roundtrip() {
        // two queries differ (random values and names); the delta must patch one into the other
        for cluster in ClusterDiscoverer::mock().get_clusters().await {
            let a = query_mock_cluster(&cluster).unwrap();
            let b = query_mock_cluster(&cluster).unwrap();
            let delta = crate::json_delta::diff(&a, &b);
            assert_eq!(crate::json_delta::tests::patch(a, &delta), b);
        }
    }
}
