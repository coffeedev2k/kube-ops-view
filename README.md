# Kubernetes Operational View

[![Build and Push Docker Image](https://github.com/thpham/kube-ops-view/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/thpham/kube-ops-view/actions/workflows/docker-publish.yml)

> This is a maintained fork of [hjacobs/kube-ops-view](https://codeberg.org/hjacobs/kube-ops-view) with modernized dependencies and additional features.

![Screenshot](screenshot.png)

Goal: provide a common operational picture for multiple Kubernetes clusters.

## Features

- Render nodes and indicate their overall status ("Ready")
- Show node capacity and resource usage (CPU, memory)
  - Render one "box" per CPU and fill up to sum of pod CPU requests/usage
  - Render vertical bar for total memory and fill up to sum of pod memory requests/usage
- **Node pool grouping with availability zone columns and color-coded visualization**
- Render individual pods
  - Indicate pod status by border line color (green: ready/running, yellow: pending, red: error etc)
  - Show current CPU/memory usage by small vertical bars
  - System pods ("kube-system" namespace) will be grouped together at the bottom
- Provide tooltip information for nodes and pods
- Animate pod creation and termination

### What's Different in This Fork

- **Rust backend**: one binary (axum, kube-rs), distroless base image
- **Node pool grouping**: Visual grouping by node pools with AZ column layout; a collapsed pool expands when a pod gets scheduled to one of its nodes
- **Zoom at the mouse pointer**: the mouse wheel zooms around the point under the pointer
- **Security focused**: Regular dependency updates via Dependabot
- **Multi-arch images**: AMD64 and ARM64 support
- **OpenShift support**: Ready-to-use deployment manifests

## What It Is Not

- It's not a replacement for the [Kubernetes Dashboard](https://github.com/kubernetes/dashboard)
- It's not a monitoring solution
- It's not an operation management tool - it's read-only

## Usage

### Running Locally

You can run the app locally with `kubectl proxy` against your running cluster:

```bash
kubectl proxy &
docker run -it --net=host ghcr.io/thpham/kube-ops-view
```

If you are using Docker for Mac:

```bash
kubectl proxy --accept-hosts '.*' &
docker run -it -p 8080:8080 -e CLUSTERS=http://docker.for.mac.localhost:8001 ghcr.io/thpham/kube-ops-view
```

Now direct your browser to http://localhost:8080

You can also try the UI with the integrated mock mode (no cluster access required):

```bash
docker run -it -p 8080:8080 ghcr.io/thpham/kube-ops-view --mock
```

### Installation

#### Kubernetes

```bash
kubectl apply -k deploy  # apply all manifests from the folder
kubectl port-forward service/kube-ops-view 8080:80
```

Now direct your browser to http://localhost:8080/

#### OpenShift

See the [openshift/](openshift/) folder for deployment options:

- `openshift/deploy/` - Basic deployment with Route
- `openshift/deploy-with-oauth-proxy/` - Deployment with OAuth proxy for authentication

```bash
kubectl apply -k openshift/deploy
```

## Development

The app can be started in "mock mode" to work on UI features without running any Kubernetes cluster:

```bash
cd app && npm install && npm start &  # watch and compile JS bundle to static/build/
cargo run -- --mock --debug
```

Without a local Rust toolchain, use `scripts/cargo` or the `Justfile` recipes (they run `cargo` in a Docker container):

```bash
just test       # cargo fmt --check, clippy, unit tests
just run-local  # build JS bundle, then run the backend in mock mode on http://localhost:8080
```

To test against a real local cluster (k3d: 1 server + 2 agents, metrics-server, demo workloads):

```bash
scripts/k3d-test.sh up       # cluster + demo pods; kubeconfig in /tmp/kov-k3d/kubeconfig.yaml
cargo run -- --kubeconfig-path /tmp/kov-k3d/kubeconfig.yaml   # local binary against the cluster
scripts/k3d-test.sh deploy   # build image, deploy deploy/ in the cluster (2 replicas + Redis)
scripts/k3d-test.sh forward  # http://localhost:8080
scripts/k3d-test.sh down
```

The script does not change `~/.kube/config` or the current kubectl context.

### Code Layout

| Path | Content |
| ---- | ------- |
| `src/main.rs` | CLI options (all options also read environment variables), startup, SIGTERM handling |
| `src/web.rs` | HTTP routes: UI page, `/events` (SSE), screen tokens, `/health`, `/static` |
| `src/oauth.rs` | OAuth 2 authorization code login |
| `src/update.rs` | Update loop: query clusters, backoff, publish `clusterupdate`/`clusterdelta`/`clusterstatus` events |
| `src/store.rs` | Memory store or Redis store (shared data, update lock, pub/sub) |
| `src/cluster_discovery.rs` | Static URLs, in-cluster service account, kubeconfig contexts, cluster registry, mock clusters |
| `src/kubernetes.rs` | Query nodes, pods, and metrics of one cluster |
| `src/json_delta.rs` | JSON diff in the format of `app/src/vendor/json_delta.js` |
| `src/mock.rs` | Mock cluster data |
| `templates/`, `static/` | HTML templates (compiled into the binary) and static files (served from `STATIC_DIR`) |
| `app/` | Frontend (JavaScript, PixiJS) |

## Building

Build the Docker image using the provided `Justfile`:

```bash
just
```

Or build directly with Docker:

```bash
docker build -t kube-ops-view .
```

## Kubernetes API Load and Memory

kube-ops-view watches nodes and pods like a client-go informer: one LIST (`resourceVersion=0`, served from the
apiserver cache), then a WATCH stream with only the changes. Metrics come from metrics.k8s.io every `METRICS_INTERVAL`.
Only the fields the UI needs stay in memory. The RBAC role needs the verbs `list` and `watch` on nodes and pods
(see `deploy/rbac.yaml`); without `watch`, kube-ops-view lists nodes and pods every 10 seconds.

Measured with a fake API (150 nodes, 6000 pods of about 10 KB each, 20 pod changes per second):

| | Before (LIST every query) | Now (watch) |
| --- | --- | --- |
| Memory (RSS) | 906 MB, growing | 130 MB, stable |
| API requests per minute | 39 (4 per query) | 8 (metrics) + rare watch reconnects |
| Data from the API in 3 minutes | 1525 MB | 95 MB, most of it the first LIST |

## Multiple Clusters

Multiple clusters are supported by passing a list of API servers, reading a kubeconfig file, or pointing to an HTTP Cluster Registry endpoint.

See the [documentation on multiple clusters](https://kubernetes-operational-view.readthedocs.io/en/latest/multiple-clusters.html) for details.

## Configuration

The following environment variables are supported:

| Variable                 | Description                                                                                        |
| ------------------------ | -------------------------------------------------------------------------------------------------- |
| `CLUSTERS`               | Comma separated list of Kubernetes API server URLs. Defaults to `http://localhost:8001/`           |
| `KUBECONFIG_PATH`        | Path to kubeconfig file to use for cluster access                                                  |
| `KUBECONFIG_CONTEXTS`    | Comma separated list of contexts to use from kubeconfig                                            |
| `CLUSTER_REGISTRY_URL`   | URL to cluster registry returning list of Kubernetes clusters                                      |
| `QUERY_INTERVAL`         | Interval in seconds for updating the UI data from the watch cache, no API requests (default: 5)   |
| `METRICS_INTERVAL`       | Interval in seconds for querying metrics.k8s.io (default: 15, the metrics-server resolution)       |
| `REDIS_URL`              | Optional Redis server for pub/sub when running multiple replicas. Example: `redis://my-redis:6379` |
| `SERVER_PORT`            | HTTP port to listen on (default: `8080`)                                                           |
| `DEBUG`                  | Set to "true" for debug logs and JavaScript cache busting in local development                     |
| `MOCK`                   | Set to "true" to mock Kubernetes cluster data                                                      |
| `ROUTE_PREFIX`           | URL prefix for reverse proxy setups                                                                |
| `SECRET_KEY`             | Secret key to sign session cookies (default: `development`, change it when you use OAuth)          |
| `STATIC_DIR`             | Directory with static files and the JavaScript bundle in `build/` (default: `static`)             |
| `RUST_LOG`               | Log filter, for example `info,kube_ops_view=debug` (overrides `DEBUG`)                             |
| `NODE_LINK_URL_TEMPLATE` | Template to make Nodes clickable. Variables: `{cluster}`, `{name}`                                 |
| `POD_LINK_URL_TEMPLATE`  | Template to make Pods clickable. Variables: `{cluster}`, `{namespace}`, `{name}`                   |

### OAuth Configuration

| Variable           | Description                                             |
| ------------------ | ------------------------------------------------------- |
| `AUTHORIZE_URL`    | OAuth 2 authorization endpoint URL                      |
| `ACCESS_TOKEN_URL` | Token endpoint URL for OAuth 2 Authorization Code Grant |
| `SCOPE`            | OAuth scope for access level                            |
| `CREDENTIALS_DIR`  | Directory to read OAuth credentials from (`authcode-client-id`, `authcode-client-secret`; `read-only-token-secret` for the cluster registry) |

## Supported Browsers

The UI uses WebGL, ECMAScript 6, and EventSource features:

- Chrome/Chromium 53.0+
- Mozilla Firefox 49.0+

## Contributing

PRs are welcome! Please open an issue first to discuss significant changes.

## Acknowledgments

This project is a fork of [kube-ops-view](https://codeberg.org/hjacobs/kube-ops-view) originally created by [Henning Jacobs](https://codeberg.org/hjacobs).

## License

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with this program. If not, see <http://www.gnu.org/licenses/>.
