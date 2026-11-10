//! Kubernetes Operational View - read-only system dashboard for multiple K8s clusters.

mod cluster_cache;
mod cluster_discovery;
mod json_delta;
mod kubernetes;
mod mock;
mod oauth;
mod store;
mod update;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Parser};
use tracing::{error, info};

use crate::cluster_discovery::{ClusterDiscoverer, DEFAULT_CLUSTERS};
use crate::store::Store;
use crate::update::ClusterQuery;

/// mimalloc returns freed memory to the OS better than glibc malloc after the large temporary
/// allocations of each update (cluster data copy, delta), so RSS stays lower.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Version string; the Docker build sets `KUBE_OPS_VIEW_VERSION` from the `VERSION` build argument.
pub const VERSION: &str = match option_env!("KUBE_OPS_VIEW_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Seconds to wait after SIGTERM before exit, so Kubernetes can remove the pod from the endpoints.
const SHUTDOWN_GRACE_SECONDS: u64 = 10;

#[derive(Parser, Debug)]
#[command(
    name = "kube-ops-view",
    about = "Kubernetes Operational View - read-only system dashboard for multiple K8s clusters",
    disable_version_flag = true
)]
struct Cli {
    /// Print the current version number and exit.
    #[arg(short = 'V', long = "version", action = ArgAction::SetTrue)]
    version: bool,

    /// HTTP port to listen on
    #[arg(short, long, env = "SERVER_PORT", default_value_t = 8080)]
    port: u16,

    /// The URL prefix under which kube-ops-view is externally reachable (for example, if
    /// kube-ops-view is served via a reverse proxy). Used for generating relative and absolute
    /// links back to kube-ops-view itself.
    #[arg(long, env = "ROUTE_PREFIX", default_value = "/")]
    route_prefix: String,

    /// Run in debugging mode
    #[arg(short, long, env = "DEBUG", action = ArgAction::SetTrue, value_parser = BoolishValueParser::new())]
    debug: bool,

    /// Mock Kubernetes clusters
    #[arg(short, long, env = "MOCK", action = ArgAction::SetTrue, value_parser = BoolishValueParser::new())]
    mock: bool,

    /// Secret key for session cookies
    #[arg(
        long,
        env = "SECRET_KEY",
        default_value = "development",
        hide_env_values = true
    )]
    secret_key: String,

    /// Redis URL to use for pub/sub and job locking
    #[arg(long, env = "REDIS_URL")]
    redis_url: Option<String>,

    /// Comma separated list of Kubernetes API server URLs (default: in-cluster service account, else http://localhost:8001/)
    #[arg(long, env = "CLUSTERS", value_delimiter = ',')]
    clusters: Vec<String>,

    /// URL to cluster registry
    #[arg(long, env = "CLUSTER_REGISTRY_URL")]
    cluster_registry_url: Option<String>,

    /// Path to kubeconfig file
    #[arg(long, env = "KUBECONFIG_PATH", value_parser = existing_path)]
    kubeconfig_path: Option<PathBuf>,

    /// List of kubeconfig contexts to use (default: use all defined contexts)
    #[arg(long, env = "KUBECONFIG_CONTEXTS", value_delimiter = ',')]
    kubeconfig_contexts: Vec<String>,

    /// Interval in seconds for updating the UI cluster data (from the watch cache, no API requests)
    #[arg(long, env = "QUERY_INTERVAL", default_value_t = 5.0)]
    query_interval: f64,

    /// Interval in seconds for querying metrics.k8s.io (metrics-server resolution is 15 s by default)
    #[arg(long, env = "METRICS_INTERVAL", default_value_t = 15.0)]
    metrics_interval: f64,

    /// Template for target URL when clicking on a Node
    #[arg(long, env = "NODE_LINK_URL_TEMPLATE")]
    node_link_url_template: Option<String>,

    /// Template for target URL when clicking on a Pod
    #[arg(long, env = "POD_LINK_URL_TEMPLATE")]
    pod_link_url_template: Option<String>,

    /// Directory with static files (favicon, font, JavaScript bundle in build/)
    #[arg(long, env = "STATIC_DIR", default_value = "static")]
    static_dir: PathBuf,

    /// OAuth2 authorization URL; enables login for the UI
    #[arg(long, env = "AUTHORIZE_URL")]
    authorize_url: Option<String>,

    /// OAuth2 access token URL
    #[arg(long, env = "ACCESS_TOKEN_URL")]
    access_token_url: Option<String>,

    /// OAuth2 scope
    #[arg(long, env = "SCOPE")]
    scope: Option<String>,

    /// Directory with credential files (authcode-client-id, authcode-client-secret, read-only-token-secret)
    #[arg(long, env = "CREDENTIALS_DIR")]
    credentials_dir: Option<PathBuf>,
}

fn existing_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.exists() {
        Ok(path)
    } else {
        Err(format!("Path '{value}' does not exist."))
    }
}

fn non_empty(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn init_logging(debug: bool) {
    let default_filter = if debug {
        "info,kube_ops_view=debug"
    } else {
        "info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.version {
        println!("Kubernetes Operational View {VERSION}");
        return ExitCode::SUCCESS;
    }
    init_logging(cli.debug);
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("Startup failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // kube and reqwest both use rustls; select one crypto provider for the whole process
    let _ = rustls::crypto::ring::default_provider().install_default();

    let store = Arc::new(match &cli.redis_url {
        Some(url) => Store::redis(url).await?,
        None => Store::memory(),
    });

    let metrics_interval = Duration::from_secs_f64(cli.metrics_interval.max(1.0));
    let (discoverer, cluster_query) = if cli.mock {
        (ClusterDiscoverer::mock(), ClusterQuery::Mock)
    } else if let Some(url) = &cli.cluster_registry_url {
        (
            ClusterDiscoverer::registry(url, &cli.credentials_dir.clone().unwrap_or_default())?,
            ClusterQuery::Kubernetes { metrics_interval },
        )
    } else if let Some(path) = &cli.kubeconfig_path {
        (
            ClusterDiscoverer::kubeconfig(path.clone(), non_empty(cli.kubeconfig_contexts.clone())),
            ClusterQuery::Kubernetes { metrics_interval },
        )
    } else {
        let urls = non_empty(cli.clusters.clone());
        if urls.is_empty() {
            info!("No --clusters given, try in-cluster service account, else {DEFAULT_CLUSTERS}");
        }
        (
            ClusterDiscoverer::static_urls(&urls).await?,
            ClusterQuery::Kubernetes { metrics_interval },
        )
    };

    tokio::spawn(update::update_clusters(
        Arc::new(discoverer),
        cluster_query,
        store.clone(),
        cli.query_interval,
    ));

    let oauth_config = match (&cli.authorize_url, &cli.access_token_url) {
        (Some(authorize_url), Some(access_token_url)) => Some(oauth::OAuthConfig {
            authorize_url: authorize_url.clone(),
            access_token_url: access_token_url.clone(),
            scope: cli.scope.clone(),
            credentials_dir: cli.credentials_dir.clone().unwrap_or_default(),
        }),
        (Some(_), None) => anyhow::bail!("AUTHORIZE_URL is set, but ACCESS_TOKEN_URL is missing"),
        _ => None,
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let web_config = web::WebConfig {
        route_prefix: cli.route_prefix.clone(),
        node_link_url_template: cli.node_link_url_template.clone(),
        pod_link_url_template: cli.pod_link_url_template.clone(),
        static_dir: cli.static_dir.clone(),
        debug: cli.debug,
        secret_key: cli.secret_key.clone(),
        oauth: oauth_config,
    };
    let router = web::router(web_config, store, shutdown.clone())?;

    tokio::spawn(handle_signals(shutdown));

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Listening on :{}..", cli.port);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// SIGTERM: report unhealthy, wait for Kubernetes to update endpoints, then exit.
/// This needs the readinessProbe periodSeconds/failureThreshold to fit in the grace period.
/// SIGINT: exit at once.
async fn handle_signals(shutdown: Arc<AtomicBool>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut sigterm), Ok(mut sigint)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        error!("Failed to install signal handlers; graceful shutdown is not available");
        return;
    };
    tokio::select! {
        _ = sigterm.recv() => {
            info!("Received TERM signal, shutting down..");
            shutdown.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(SHUTDOWN_GRACE_SECONDS)).await;
        }
        _ = sigint.recv() => {
            info!("Received INT signal, exit now");
        }
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_defaults_and_lists() {
        let cli = Cli::try_parse_from(["kube-ops-view"]).unwrap();
        assert_eq!(cli.port, 8080);
        assert_eq!(cli.route_prefix, "/");
        assert!(!cli.mock);
        let cli =
            Cli::try_parse_from(["kube-ops-view", "-m", "--clusters", "https://a,,https://b"])
                .unwrap();
        assert!(cli.mock);
        assert_eq!(non_empty(cli.clusters), vec!["https://a", "https://b"]);
    }
}
