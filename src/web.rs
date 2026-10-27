//! HTTP server: UI page, SSE event stream, screen tokens, health check, static files.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, FromRef, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::{Cookie, Key, SameSite, SignedCookieJar};
use futures::Stream;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use crate::oauth::{self, OAuthConfig};
use crate::store::{Store, StoreMessage};

const INDEX_TEMPLATE: &str = include_str!("../templates/index.html");
const SCREEN_TOKENS_TEMPLATE: &str = include_str!("../templates/screen-tokens.html");

/// Signed session cookie; value is `screen:<token>` or `oauth`.
pub const SESSION_COOKIE: &str = "session";
pub const SESSION_OAUTH: &str = "oauth";
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

pub struct WebConfig {
    pub route_prefix: String,
    pub node_link_url_template: Option<String>,
    pub pod_link_url_template: Option<String>,
    pub static_dir: PathBuf,
    pub debug: bool,
    pub secret_key: String,
    pub oauth: Option<OAuthConfig>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<WebConfig>,
    pub store: Arc<Store>,
    pub shutdown: Arc<AtomicBool>,
    pub http: reqwest::Client,
    cookie_key: Key,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.cookie_key.clone()
    }
}

impl AppState {
    /// Path (or URL) of `path` under ROUTE_PREFIX, for links and redirects.
    pub fn prefixed(&self, path: &str) -> String {
        let prefix = &self.config.route_prefix;
        if prefix.ends_with('/') {
            format!("{prefix}{path}")
        } else {
            format!("{prefix}/{path}")
        }
    }
}

pub fn session_cookie(name: &'static str, value: String) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .build()
}

pub fn router(
    config: WebConfig,
    store: Arc<Store>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<Router> {
    // SECRET_KEY can be short ("development"); the signing key needs 64 bytes
    let cookie_key = Key::from(&Sha512::digest(config.secret_key.as_bytes()));
    let static_dir = config.static_dir.clone();
    if !static_dir.join("build").is_dir() {
        warn!(
            "Static directory {} has no build/ (JavaScript bundle); the UI will return 503",
            static_dir.display()
        );
    }
    let state = AppState {
        config: Arc::new(config),
        store,
        shutdown,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
        cookie_key,
    };

    let protected = Router::new()
        .route("/", get(index))
        .route("/events", get(events))
        .route(
            "/screen-tokens",
            get(screen_tokens).post(create_screen_token),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));

    Ok(Router::new()
        .merge(protected)
        .route("/health", get(health))
        .route("/screen/{token}", get(redeem_screen_token))
        .route("/logout", get(logout))
        .route(&format!("/{}", oauth::LOGIN_PATH), get(oauth::login))
        .route("/login/oauth/authorized", get(oauth::authorized))
        .nest_service("/static", ServeDir::new(static_dir))
        .with_state(state))
}

/// With OAuth enabled, the UI needs a session (OAuth login or redeemed screen token).
async fn authorize(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    request: Request,
    next: Next,
) -> Response {
    if state.config.oauth.is_some() && jar.get(SESSION_COOKIE).is_none() {
        return Redirect::to(&state.prefixed(oauth::LOGIN_PATH)).into_response();
    }
    next.run(request).await
}

async fn health(State(state): State<AppState>) -> Response {
    if state.shutdown.load(Ordering::SeqCst) {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        "OK".into_response()
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// First `app*.js` file in the build directory (webpack adds a content hash to the name).
fn find_app_js(static_dir: &std::path::Path) -> Option<String> {
    let mut candidates: Vec<String> = std::fs::read_dir(static_dir.join("build"))
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("app") && name.ends_with(".js"))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

async fn index(State(state): State<AppState>) -> Response {
    let config = &state.config;
    let Some(mut app_js) = find_app_js(&config.static_dir) else {
        error!(
            "Could not find JavaScript application bundle app*.js in {}",
            config.static_dir.join("build").display()
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "JavaScript application bundle not found (missing build)",
        )
            .into_response();
    };
    if config.debug {
        // cache busting for local development
        app_js = format!("{app_js}?_={}", crate::store::unix_now());
    }
    let app_config = serde_json::json!({
        "node_link_url_template": config.node_link_url_template,
        "pod_link_url_template": config.pod_link_url_template,
        "route_prefix": config.route_prefix,
    });
    // "<" only occurs in JSON strings, where < is the same character; this keeps </script> out
    let app_config_json = app_config.to_string().replace('<', "\\u003c");
    let html = INDEX_TEMPLATE
        .replace("{{ version }}", &html_escape(crate::VERSION))
        .replace("{{ route_prefix }}", &html_escape(&state.prefixed("")))
        .replace("{{ app_js }}", &html_escape(&app_js))
        .replace("{{ app_config_json }}", &app_config_json);
    Html(html).into_response()
}

#[derive(Deserialize)]
struct EventsParams {
    #[serde(default)]
    cluster_ids: String,
}

/// SSE (Server-Sent Events) stream for the UI EventSource.
///
/// First the full data of each cluster (`clusterstatus`, `clusterupdate`), then `bootstrapend`,
/// then the live events. `cluster_ids` (comma or space separated) filters the clusters.
async fn events(State(state): State<AppState>, Query(params): Query<EventsParams>) -> Response {
    let cluster_ids: HashSet<String> = params
        .cluster_ids
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let headers = [
        (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        (
            header::HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        ),
    ];
    let stream = event_stream(state.store.clone(), cluster_ids);
    (
        headers,
        Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE)),
    )
        .into_response()
}

fn event_stream(
    store: Arc<Store>,
    cluster_ids: HashSet<String>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::spawn(async move {
        // subscribe before the bootstrap read, so no event between bootstrap and live stream is lost
        let mut live = store.subscribe();
        let wanted = |id: &str| cluster_ids.is_empty() || cluster_ids.contains(id);

        match bootstrap_events(&store, &wanted).await {
            Ok(events) => {
                for event in events {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                error!("Failed to read cluster data for the event stream: {e:#}");
                return;
            }
        }
        if tx
            .send(Event::default().event("bootstrapend").data(""))
            .await
            .is_err()
        {
            return;
        }

        loop {
            match live.recv().await {
                Ok(StoreMessage::Event(e)) => {
                    if !cluster_ids.is_empty() && !e.cluster_id.as_deref().is_some_and(wanted) {
                        continue;
                    }
                    let event = Event::default().event(&e.event_type).data(&e.data_json);
                    if tx.send(event).await.is_err() {
                        return; // client disconnected
                    }
                }
                Ok(StoreMessage::Resync) => {
                    info!(
                        "Event stream had a gap, close the client stream so that the UI reconnects"
                    );
                    return;
                }
                Err(RecvError::Lagged(n)) => {
                    warn!(
                        "Event stream client is too slow, lost {n} events, close the stream so that the UI reconnects"
                    );
                    return;
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    })
}

async fn bootstrap_events(
    store: &Store,
    wanted: &impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<Event>> {
    let mut events = Vec::new();
    for cluster_id in store.get_cluster_ids().await? {
        if !wanted(&cluster_id) {
            continue;
        }
        let status = store.get_cluster_status(&cluster_id).await?;
        if status != Default::default() {
            // send the cluster status including last_query_time BEFORE the cluster data
            // so the UI knows how to render correctly from the start
            let data = serde_json::json!({"cluster_id": cluster_id, "status": status});
            events.push(
                Event::default()
                    .event("clusterstatus")
                    .data(data.to_string()),
            );
        }
        if let Some(data) = store.get_cluster_data(&cluster_id).await? {
            events.push(
                Event::default()
                    .event("clusterupdate")
                    .data(serde_json::to_string(&*data)?),
            );
        }
    }
    Ok(events)
}

fn screen_tokens_page(state: &AppState, new_token: Option<&str>) -> Html<String> {
    let new_token_html = new_token
        .map(|t| format!("<p>The new token is: <code>{}</code></p>", html_escape(t)))
        .unwrap_or_default();
    Html(
        SCREEN_TOKENS_TEMPLATE
            .replace("{{ route_prefix }}", &html_escape(&state.prefixed("")))
            .replace("{{ new_token_html }}", &new_token_html),
    )
}

async fn screen_tokens(State(state): State<AppState>) -> Html<String> {
    screen_tokens_page(&state, None)
}

async fn create_screen_token(State(state): State<AppState>) -> Response {
    match state.store.create_screen_token().await {
        Ok(token) => screen_tokens_page(&state, Some(&token)).into_response(),
        Err(e) => {
            error!("Failed to create screen token: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn redeem_screen_token(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Path(token): Path<String>,
) -> Response {
    let remote_addr = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| peer.ip().to_string());
    info!("Trying to redeem screen token \"{token}\" for IP {remote_addr}..");
    if let Err(e) = state.store.redeem_screen_token(&token, &remote_addr).await {
        warn!("Screen token \"{token}\" rejected for IP {remote_addr}: {e:#}");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let jar = jar.add(session_cookie(SESSION_COOKIE, format!("screen:{token}")));
    (jar, Redirect::to(&state.config.route_prefix)).into_response()
}

async fn logout(State(state): State<AppState>, jar: SignedCookieJar) -> Response {
    let jar = jar.remove(Cookie::build(SESSION_COOKIE).path("/"));
    (jar, Redirect::to(&state.config.route_prefix)).into_response()
}
