#!/usr/bin/env bash
# Local test cluster for kube-ops-view (k3d, 1 server + 2 agents, metrics-server included).
#
#   scripts/k3d-test.sh up       create the cluster, label zones, deploy demo workloads
#   scripts/k3d-test.sh deploy   build the image, import it, apply deploy/ (2 replicas + Redis)
#   scripts/k3d-test.sh forward  port-forward the service to http://localhost:8080
#   scripts/k3d-test.sh down     delete the cluster
#
# The cluster kubeconfig goes to $KOV_K3D_DIR/kubeconfig.yaml only:
# ~/.kube/config and the current kubectl context do not change (also on `down`).
set -euo pipefail

CLUSTER="${KOV_K3D_CLUSTER:-kov-test}"
DIR="${KOV_K3D_DIR:-/tmp/kov-k3d}"
KUBECONFIG_FILE="${DIR}/kubeconfig.yaml"
TAG="${KOV_K3D_TAG:-k3d-$(date +%s)}"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# k3d reads and REWRITES the default kubeconfig on `cluster delete` (even without k3d entries):
# point it to the cluster's own file for all commands.
export KUBECONFIG="${KUBECONFIG_FILE}"

k() { kubectl --kubeconfig "${KUBECONFIG_FILE}" --context "k3d-${CLUSTER}" "$@"; }

up() {
    mkdir -p "${DIR}"
    k3d cluster create "${CLUSTER}" --agents 2 \
        --kubeconfig-update-default=false --kubeconfig-switch-context=false \
        --wait --timeout 300s
    k3d kubeconfig get "${CLUSTER}" > "${KUBECONFIG_FILE}"
    chmod 600 "${KUBECONFIG_FILE}"
    k label node "k3d-${CLUSTER}-server-0" topology.kubernetes.io/zone=zone-a --overwrite
    k label node "k3d-${CLUSTER}-agent-0" topology.kubernetes.io/zone=zone-b node-role.kubernetes.io/worker= --overwrite
    k label node "k3d-${CLUSTER}-agent-1" topology.kubernetes.io/zone=zone-c node-role.kubernetes.io/worker= --overwrite
    k -n kube-system rollout status deploy/metrics-server --timeout=180s
    k apply -f - <<'EOF'
apiVersion: v1
kind: Namespace
metadata: {name: kov-demo}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: kov-demo}
spec:
  replicas: 4
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web, env: prod}}
    spec:
      containers:
        - name: nginx
          image: nginx:alpine
          resources: {requests: {cpu: 50m, memory: 32Mi}, limits: {memory: 64Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: crashloop, namespace: kov-demo}
spec:
  containers:
    - {name: main, image: busybox, command: ["sh", "-c", "sleep 2; exit 1"]}
---
apiVersion: v1
kind: Pod
metadata: {name: badimage, namespace: kov-demo}
spec:
  containers:
    - {name: main, image: "example.invalid/does-not-exist:1"}
---
apiVersion: v1
kind: Pod
metadata: {name: unschedulable, namespace: kov-demo}
spec:
  nodeSelector: {kov: no-such-node}
  containers:
    - {name: main, image: busybox, command: ["sleep", "3600"]}
EOF
    echo "Cluster ${CLUSTER} is ready. kubeconfig: ${KUBECONFIG_FILE}"
    echo "Run the local binary: cargo run -- --kubeconfig-path ${KUBECONFIG_FILE}"
}

deploy() {
    docker build --build-arg "VERSION=${TAG}" -t "kube-ops-view:${TAG}" "${REPO_DIR}"
    # Docker with the containerd image store adds attestation manifests; `k3d image import`
    # then reports success but the nodes have no image. Import one platform with ctr instead.
    docker save --platform linux/amd64 "kube-ops-view:${TAG}" -o "${DIR}/image.tar"
    for node in $(k3d node list -o json | python3 -c "import json,sys; print(' '.join(n['name'] for n in json.load(sys.stdin) if n['role'] in ('server','agent') and n['runtimeLabels'].get('k3d.cluster') == '${CLUSTER}'))"); do
        docker cp "${DIR}/image.tar" "${node}:/tmp/image.tar"
        docker exec "${node}" ctr -n k8s.io images import /tmp/image.tar
        docker exec "${node}" rm /tmp/image.tar
    done
    rm -f "${DIR}/image.tar"
    rm -rf "${DIR}/deploy"
    cp -r "${REPO_DIR}/deploy" "${DIR}/deploy"
    cat >> "${DIR}/deploy/kustomization.yaml" <<EOF
images:
  - name: ghcr.io/thpham/kube-ops-view
    newName: docker.io/library/kube-ops-view
    newTag: ${TAG}
replicas:
  - name: kube-ops-view
    count: 2
EOF
    k apply -k "${DIR}/deploy"
    k rollout status deploy/kube-ops-view-redis --timeout=120s
    k rollout status deploy/kube-ops-view --timeout=120s
}

forward() {
    k port-forward svc/kube-ops-view 8080:80
}

down() {
    k3d cluster delete "${CLUSTER}"
    rm -rf "${DIR}"
}

case "${1:-}" in
    up | deploy | forward | down) "$1" ;;
    *)
        echo "usage: $0 up|deploy|forward|down" >&2
        exit 2
        ;;
esac
