//! Cluster discovery: static API server URLs, cluster registry, kubeconfig contexts, or mock clusters.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use kube::config::{KubeConfigOptions, Kubeconfig};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::cluster_cache::{self, ClusterCache, ClusterWatches};

/// Default URL points to kubectl proxy.
pub const DEFAULT_CLUSTERS: &str = "http://localhost:8001/";
const REGISTRY_CACHE_LIFETIME: Duration = Duration::from_secs(60);
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(10);
/// Name of the OAuth token file `<CREDENTIALS_DIR>/<name>-token-secret` for cluster registry and its clusters.
const REGISTRY_TOKEN_NAME: &str = "read-only";
const KUBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KUBE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const MOCK_CLUSTER_COUNT: usize = 3;

pub struct Cluster {
    pub id: String,
    pub name: String,
    pub api_server_url: String,
    /// `None` for mock clusters.
    pub client: Option<kube::Client>,
    /// Watch-backed cache, started on the first query; dropping the cluster stops the watches.
    watches: OnceLock<ClusterWatches>,
}

impl Cluster {
    pub fn new(
        id: String,
        name: String,
        api_server_url: String,
        client: Option<kube::Client>,
    ) -> Self {
        Cluster {
            id,
            name,
            api_server_url,
            client,
            watches: OnceLock::new(),
        }
    }

    /// The watch-backed cache of this cluster; starts the watches on the first call.
    pub fn cache(&self, metrics_interval: Duration) -> anyhow::Result<&ClusterCache> {
        let client = self
            .client
            .as_ref()
            .with_context(|| format!("cluster {} has no Kubernetes client", self.id))?;
        let watches = self
            .watches
            .get_or_init(|| cluster_cache::start(&self.id, client.clone(), metrics_interval));
        Ok(&watches.cache)
    }
}

/// Generate some "cluster ID" from given API server URL.
pub fn generate_cluster_id(url: &str) -> String {
    let url = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let id: String = url
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ':' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    id.trim_matches('-').to_string()
}

fn kube_client(mut config: kube::Config) -> anyhow::Result<kube::Client> {
    config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
    config.read_timeout = Some(KUBE_READ_TIMEOUT);
    Ok(kube::Client::try_from(config)?)
}

fn url_config(api_server_url: &str) -> anyhow::Result<kube::Config> {
    let uri: http::Uri = api_server_url
        .parse()
        .with_context(|| format!("invalid API server URL {api_server_url}"))?;
    Ok(kube::Config::new(uri))
}

pub enum ClusterDiscoverer {
    Static(Vec<Arc<Cluster>>),
    Registry(RegistryDiscoverer),
    Kubeconfig(KubeconfigDiscoverer),
    Mock(Vec<Arc<Cluster>>),
}

impl ClusterDiscoverer {
    pub async fn static_urls(api_server_urls: &[String]) -> anyhow::Result<Self> {
        let mut clusters = Vec::new();
        if api_server_urls.is_empty() {
            let cluster = match kube::Config::incluster() {
                Ok(config) => {
                    let url = config.cluster_url.to_string();
                    info!("Use in-cluster service account for {url}");
                    Cluster::new(
                        generate_cluster_id(&url),
                        "cluster".to_string(),
                        url,
                        Some(kube_client(config)?),
                    )
                }
                Err(e) => {
                    // we are not running inside a cluster => assume default kubectl proxy URL
                    info!("No in-cluster config ({e}), use kubectl proxy URL {DEFAULT_CLUSTERS}");
                    Cluster::new(
                        generate_cluster_id(DEFAULT_CLUSTERS),
                        "cluster".to_string(),
                        DEFAULT_CLUSTERS.to_string(),
                        Some(kube_client(url_config(DEFAULT_CLUSTERS)?)?),
                    )
                }
            };
            clusters.push(Arc::new(cluster));
        } else {
            for url in api_server_urls {
                let id = generate_cluster_id(url);
                clusters.push(Arc::new(Cluster::new(
                    id.clone(),
                    id,
                    url.clone(),
                    Some(kube_client(url_config(url)?)?),
                )));
            }
        }
        Ok(ClusterDiscoverer::Static(clusters))
    }

    pub fn registry(url: &str, credentials_dir: &Path) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REGISTRY_TIMEOUT)
            .build()?;
        Ok(ClusterDiscoverer::Registry(RegistryDiscoverer {
            url: url.to_string(),
            token_path: credentials_dir.join(format!("{REGISTRY_TOKEN_NAME}-token-secret")),
            http,
            cache: Mutex::new(RegistryCache::default()),
        }))
    }

    pub fn kubeconfig(path: PathBuf, contexts: Vec<String>) -> Self {
        ClusterDiscoverer::Kubeconfig(KubeconfigDiscoverer {
            path,
            contexts: contexts.into_iter().collect(),
            cache: tokio::sync::Mutex::new(None),
        })
    }

    pub fn mock() -> Self {
        let clusters = (0..MOCK_CLUSTER_COUNT)
            .map(|i| {
                Arc::new(Cluster::new(
                    format!("mock-cluster-{i}"),
                    format!("mock-cluster-{i}"),
                    format!("https://kube-{i}.example.org"),
                    None,
                ))
            })
            .collect();
        ClusterDiscoverer::Mock(clusters)
    }

    pub async fn get_clusters(&self) -> Vec<Arc<Cluster>> {
        match self {
            ClusterDiscoverer::Static(clusters) | ClusterDiscoverer::Mock(clusters) => {
                clusters.clone()
            }
            ClusterDiscoverer::Registry(d) => d.get_clusters().await,
            ClusterDiscoverer::Kubeconfig(d) => d.get_clusters().await,
        }
    }
}

#[derive(Default)]
struct RegistryCache {
    last_refresh: Option<Instant>,
    clusters: Vec<Arc<Cluster>>,
    /// OAuth token of the current cluster clients.
    token: String,
}

pub struct RegistryDiscoverer {
    url: String,
    token_path: PathBuf,
    http: reqwest::Client,
    cache: Mutex<RegistryCache>,
}

#[derive(Deserialize)]
struct RegistryClusterList {
    items: Vec<RegistryCluster>,
}

#[derive(Deserialize)]
struct RegistryCluster {
    id: String,
    alias: String,
    api_server_url: String,
    lifecycle_status: Option<String>,
}

/// Fixed token from `OAUTH2_ACCESS_TOKENS` (format `name1=token1,name2=token2`).
fn fixed_token(access_tokens: &str, name: &str) -> Option<String> {
    access_tokens
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .find(|(n, _)| n.trim() == name)
        .map(|(_, token)| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

impl RegistryDiscoverer {
    /// OAuth token from `OAUTH2_ACCESS_TOKENS`, else from the token file.
    /// Read on each refresh: the file can change (for example, a mounted Kubernetes secret).
    fn read_token(&self) -> anyhow::Result<String> {
        if let Some(token) = std::env::var("OAUTH2_ACCESS_TOKENS")
            .ok()
            .and_then(|v| fixed_token(&v, REGISTRY_TOKEN_NAME))
        {
            return Ok(token);
        }
        let token = std::fs::read_to_string(&self.token_path).with_context(|| {
            format!(
                "cannot read OAuth token {} (and OAUTH2_ACCESS_TOKENS has no {REGISTRY_TOKEN_NAME}=...)",
                self.token_path.display()
            )
        })?;
        Ok(token.trim().to_string())
    }

    async fn get_clusters(&self) -> Vec<Arc<Cluster>> {
        let expired = {
            let cache = self.cache.lock().unwrap();
            cache
                .last_refresh
                .is_none_or(|t| t.elapsed() > REGISTRY_CACHE_LIFETIME)
        };
        if expired && let Err(e) = self.refresh().await {
            error!(
                "Failed to refresh from cluster registry {}: {e:#}",
                self.url
            );
        }
        self.cache.lock().unwrap().clusters.clone()
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        let token = self.read_token()?;
        let list_url = url::Url::parse(&self.url)?.join("/kubernetes-clusters")?;
        let list: RegistryClusterList = self
            .http
            .get(list_url)
            .bearer_auth(&token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // keep the existing clients for unchanged clusters if the token did not change
        let known: HashMap<String, Arc<Cluster>> = {
            let cache = self.cache.lock().unwrap();
            if cache.token == token {
                cache
                    .clusters
                    .iter()
                    .map(|c| (c.id.clone(), c.clone()))
                    .collect()
            } else {
                HashMap::new()
            }
        };
        let mut clusters = Vec::new();
        for row in list.items {
            // only consider "ready" clusters
            if row.lifecycle_status.as_deref().unwrap_or("ready") != "ready" {
                continue;
            }
            if let Some(c) = known.get(&row.id)
                && c.name == row.alias
                && c.api_server_url == row.api_server_url
            {
                clusters.push(c.clone());
                continue;
            }
            let mut config = url_config(&row.api_server_url)?;
            config.auth_info.token = Some(token.clone().into());
            clusters.push(Arc::new(Cluster::new(
                row.id,
                row.alias,
                row.api_server_url,
                Some(kube_client(config)?),
            )));
        }
        let mut cache = self.cache.lock().unwrap();
        cache.clusters = clusters;
        cache.token = token;
        cache.last_refresh = Some(Instant::now());
        Ok(())
    }
}

struct KubeconfigCache {
    content: String,
    clusters: Vec<Arc<Cluster>>,
}

pub struct KubeconfigDiscoverer {
    path: PathBuf,
    /// Empty set: use all contexts.
    contexts: HashSet<String>,
    cache: tokio::sync::Mutex<Option<KubeconfigCache>>,
}

impl KubeconfigDiscoverer {
    /// Read the kubeconfig file on each call; build new clients only if the file changed.
    async fn get_clusters(&self) -> Vec<Arc<Cluster>> {
        let mut cache = self.cache.lock().await;
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to read kubeconfig {}: {e}", self.path.display());
                return cache
                    .as_ref()
                    .map(|c| c.clusters.clone())
                    .unwrap_or_default();
            }
        };
        if let Some(c) = cache.as_ref()
            && c.content == content
        {
            return c.clusters.clone();
        }
        let clusters = match self.load(&content).await {
            Ok(clusters) => clusters,
            Err(e) => {
                error!("Failed to load kubeconfig {}: {e:#}", self.path.display());
                return cache
                    .as_ref()
                    .map(|c| c.clusters.clone())
                    .unwrap_or_default();
            }
        };
        info!(
            "Loaded {} cluster(s) from kubeconfig {}",
            clusters.len(),
            self.path.display()
        );
        *cache = Some(KubeconfigCache {
            content,
            clusters: clusters.clone(),
        });
        clusters
    }

    async fn load(&self, content: &str) -> anyhow::Result<Vec<Arc<Cluster>>> {
        let kubeconfig = Kubeconfig::from_yaml(content)?;
        let mut clusters = Vec::new();
        for context in &kubeconfig.contexts {
            let name = &context.name;
            if !self.contexts.is_empty() && !self.contexts.contains(name) {
                continue;
            }
            let options = KubeConfigOptions {
                context: Some(name.clone()),
                ..Default::default()
            };
            let config =
                match kube::Config::from_custom_kubeconfig(kubeconfig.clone(), &options).await {
                    Ok(config) => config,
                    Err(e) => {
                        warn!("Skip kubeconfig context {name}: {e}");
                        continue;
                    }
                };
            clusters.push(Arc::new(Cluster::new(
                name.clone(),
                name.clone(),
                config.cluster_url.to_string(),
                Some(kube_client(config)?),
            )));
        }
        Ok(clusters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_token_from_env_value() {
        assert_eq!(
            fixed_token("read-only=abc", "read-only").as_deref(),
            Some("abc")
        );
        assert_eq!(
            fixed_token("x=1, read-only=a=b ", "read-only").as_deref(),
            Some("a=b")
        );
        assert_eq!(fixed_token("x=1", "read-only"), None);
        assert_eq!(fixed_token("read-only=", "read-only"), None);
    }

    #[test]
    fn cluster_id_from_url() {
        assert_eq!(
            generate_cluster_id("http://localhost:8001/"),
            "localhost:8001"
        );
        assert_eq!(
            generate_cluster_id("https://Kube-1.Example.org"),
            "kube-1-example-org"
        );
        assert_eq!(generate_cluster_id("https://a..b/"), "a--b");
    }
}
