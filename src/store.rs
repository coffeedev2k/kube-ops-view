//! Store for cluster data, cluster status, screen tokens, update lock, and event pub/sub.
//!
//! - memory store: one process only, mostly useful for local debugging
//! - Redis store: deployments with replicas > 1 (shared data, update lock, pub/sub channel `default`)

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use rand::Rng;
use rand::distr::Alphanumeric;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

const ONE_YEAR_SECONDS: f64 = 3600.0 * 24.0 * 365.0;
const SCREEN_TOKEN_LENGTH: usize = 10;
/// Redis pub/sub channel for store events.
const EVENT_CHANNEL: &str = "default";
/// Redis key and TTL of the update lock (only one replica queries clusters at a time).
const UPDATE_LOCK_KEY: &str = "update";
const UPDATE_LOCK_TTL_MS: u64 = 10_000;
/// Events buffered per SSE client; a slower client gets disconnected and must reconnect.
const EVENT_BUFFER: usize = 1024;

pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// One event for the UI (`clusterupdate`, `clusterdelta`, `clusterstatus`).
#[derive(Debug)]
pub struct StoreEvent {
    pub event_type: String,
    /// `cluster_id` of the event data (or `id` for a full `clusterupdate`), used for the UI cluster filter.
    pub cluster_id: Option<String>,
    /// Compact JSON of the event data.
    pub data_json: String,
}

#[derive(Debug, Clone)]
pub enum StoreMessage {
    Event(Arc<StoreEvent>),
    /// The event stream had a gap; each SSE client must reconnect to get full data again.
    Resync,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Backoff {
    pub tries: u32,
    pub next_try: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClusterStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_query_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScreenTokenData {
    token: String,
    created: f64,
    expires: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_addr: Option<String>,
}

fn generate_token_data() -> ScreenTokenData {
    let token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(SCREEN_TOKEN_LENGTH)
        .map(char::from)
        .collect();
    let now = unix_now();
    ScreenTokenData {
        token,
        created: now,
        expires: now + ONE_YEAR_SECONDS,
        remote_addr: None,
    }
}

/// Check that the screen token is valid and bind it to the first IP that redeems it.
fn check_token(
    data: Option<ScreenTokenData>,
    remote_addr: &str,
) -> anyhow::Result<ScreenTokenData> {
    match data {
        Some(mut data)
            if unix_now() < data.expires
                && data.remote_addr.as_deref().unwrap_or(remote_addr) == remote_addr =>
        {
            data.remote_addr = Some(remote_addr.to_string());
            Ok(data)
        }
        _ => anyhow::bail!("Invalid token"),
    }
}

fn event_cluster_id(data: &Value) -> Option<String> {
    data.get("cluster_id")
        .or_else(|| data.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub enum UpdateLock {
    Memory,
    Redis(String),
}

enum Backend {
    Memory {
        values: Mutex<HashMap<String, Arc<Value>>>,
        screen_tokens: Mutex<HashMap<String, ScreenTokenData>>,
    },
    Redis {
        conn: ConnectionManager,
    },
}

pub struct Store {
    backend: Backend,
    events: broadcast::Sender<StoreMessage>,
}

impl Store {
    pub fn memory() -> Self {
        Store {
            backend: Backend::Memory {
                values: Mutex::new(HashMap::new()),
                screen_tokens: Mutex::new(HashMap::new()),
            },
            events: broadcast::channel(EVENT_BUFFER).0,
        }
    }

    pub async fn redis(url: &str) -> anyhow::Result<Self> {
        info!("Connecting to Redis on {url}..");
        let client = redis::Client::open(url).context("invalid Redis URL")?;
        let conn = client
            .get_connection_manager()
            .await
            .context("failed to connect to Redis")?;
        let events = broadcast::channel(EVENT_BUFFER).0;
        tokio::spawn(redis_event_listener(client, events.clone()));
        Ok(Store {
            backend: Backend::Redis { conn },
            events,
        })
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Arc<Value>>> {
        match &self.backend {
            Backend::Memory { values, .. } => Ok(values.lock().unwrap().get(key).cloned()),
            Backend::Redis { conn } => {
                let raw: Option<String> = conn.clone().get(key).await?;
                match raw {
                    Some(raw) => Ok(Some(Arc::new(serde_json::from_str(&raw)?))),
                    None => Ok(None),
                }
            }
        }
    }

    async fn set(&self, key: &str, value: Arc<Value>) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Memory { values, .. } => {
                values.lock().unwrap().insert(key.to_string(), value);
            }
            Backend::Redis { conn } => {
                let raw = serde_json::to_string(&*value)?;
                let _: () = conn.clone().set(key, raw).await?;
            }
        }
        Ok(())
    }

    pub async fn get_cluster_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(match self.get("cluster-ids").await? {
            Some(v) => serde_json::from_value((*v).clone())?,
            None => Vec::new(),
        })
    }

    pub async fn set_cluster_ids(&self, cluster_ids: &BTreeSet<String>) -> anyhow::Result<()> {
        self.set("cluster-ids", Arc::new(serde_json::to_value(cluster_ids)?))
            .await
    }

    pub async fn get_cluster_status(&self, cluster_id: &str) -> anyhow::Result<ClusterStatus> {
        Ok(
            match self.get(&format!("clusters:{cluster_id}:status")).await? {
                Some(v) => serde_json::from_value((*v).clone())?,
                None => ClusterStatus::default(),
            },
        )
    }

    pub async fn set_cluster_status(
        &self,
        cluster_id: &str,
        status: &ClusterStatus,
    ) -> anyhow::Result<()> {
        self.set(
            &format!("clusters:{cluster_id}:status"),
            Arc::new(serde_json::to_value(status)?),
        )
        .await
    }

    pub async fn get_cluster_data(&self, cluster_id: &str) -> anyhow::Result<Option<Arc<Value>>> {
        self.get(&format!("clusters:{cluster_id}:data")).await
    }

    pub async fn set_cluster_data(&self, cluster_id: &str, data: Arc<Value>) -> anyhow::Result<()> {
        self.set(&format!("clusters:{cluster_id}:data"), data).await
    }

    pub async fn acquire_lock(&self) -> anyhow::Result<Option<UpdateLock>> {
        match &self.backend {
            Backend::Memory { .. } => Ok(Some(UpdateLock::Memory)),
            Backend::Redis { conn } => {
                let lock_value: String = rand::rng()
                    .sample_iter(&Alphanumeric)
                    .take(22)
                    .map(char::from)
                    .collect();
                let acquired: Option<String> = redis::cmd("SET")
                    .arg(UPDATE_LOCK_KEY)
                    .arg(&lock_value)
                    .arg("NX")
                    .arg("PX")
                    .arg(UPDATE_LOCK_TTL_MS)
                    .query_async(&mut conn.clone())
                    .await?;
                Ok(acquired.map(|_| UpdateLock::Redis(lock_value)))
            }
        }
    }

    pub async fn release_lock(&self, lock: UpdateLock) -> anyhow::Result<()> {
        if let (Backend::Redis { conn }, UpdateLock::Redis(lock_value)) = (&self.backend, lock) {
            // delete the lock only if this process still owns it
            let script = redis::Script::new(
                r#"if redis.call("get", KEYS[1]) == ARGV[1] then return redis.call("del", KEYS[1]) else return 0 end"#,
            );
            let _: i64 = script
                .key(UPDATE_LOCK_KEY)
                .arg(lock_value)
                .invoke_async(&mut conn.clone())
                .await?;
        }
        Ok(())
    }

    pub async fn publish(&self, event_type: &str, data: &Value) -> anyhow::Result<()> {
        let data_json = serde_json::to_string(data)?;
        match &self.backend {
            Backend::Memory { .. } => {
                // no receivers is not an error: no UI is connected
                let _ = self.events.send(StoreMessage::Event(Arc::new(StoreEvent {
                    event_type: event_type.to_string(),
                    cluster_id: event_cluster_id(data),
                    data_json,
                })));
            }
            Backend::Redis { conn } => {
                // the Redis listener gives the event back to the local subscribers
                let _: i64 = conn
                    .clone()
                    .publish(EVENT_CHANNEL, format!("{event_type}:{data_json}"))
                    .await?;
            }
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoreMessage> {
        self.events.subscribe()
    }

    pub async fn create_screen_token(&self) -> anyhow::Result<String> {
        let data = generate_token_data();
        let token = data.token.clone();
        match &self.backend {
            Backend::Memory { screen_tokens, .. } => {
                screen_tokens.lock().unwrap().insert(token.clone(), data);
            }
            Backend::Redis { conn } => {
                let _: () = conn
                    .clone()
                    .set(
                        format!("screen-tokens:{token}"),
                        serde_json::to_string(&data)?,
                    )
                    .await?;
            }
        }
        Ok(token)
    }

    pub async fn redeem_screen_token(&self, token: &str, remote_addr: &str) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Memory { screen_tokens, .. } => {
                let mut tokens = screen_tokens.lock().unwrap();
                let data = check_token(tokens.get(token).cloned(), remote_addr)?;
                tokens.insert(token.to_string(), data);
            }
            Backend::Redis { conn } => {
                let key = format!("screen-tokens:{token}");
                let raw: Option<String> = conn.clone().get(&key).await?;
                let data = raw.map(|r| serde_json::from_str(&r)).transpose()?;
                let data = check_token(data, remote_addr)?;
                let _: () = conn
                    .clone()
                    .set(&key, serde_json::to_string(&data)?)
                    .await?;
            }
        }
        Ok(())
    }
}

/// Forward Redis pub/sub messages to the local subscribers; reconnect on failure.
async fn redis_event_listener(client: redis::Client, events: broadcast::Sender<StoreMessage>) {
    loop {
        match listen_once(&client, &events).await {
            Ok(()) => warn!("Redis pub/sub stream ended, reconnect.."),
            Err(e) => error!("Redis pub/sub failed: {e:#}, reconnect in 1 second.."),
        }
        // events were possibly lost: SSE clients must get full data again
        let _ = events.send(StoreMessage::Resync);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn listen_once(
    client: &redis::Client,
    events: &broadcast::Sender<StoreMessage>,
) -> anyhow::Result<()> {
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(EVENT_CHANNEL).await?;
    info!("Subscribed to Redis channel {EVENT_CHANNEL}");
    let mut messages = pubsub.on_message();
    while let Some(msg) = messages.next().await {
        let payload: String = msg.get_payload()?;
        let Some((event_type, data_json)) = payload.split_once(':') else {
            warn!("Ignore Redis event without 'type:' prefix: {payload:.100}");
            continue;
        };
        let cluster_id = match serde_json::from_str::<Value>(data_json) {
            Ok(data) => event_cluster_id(&data),
            Err(e) => {
                warn!("Ignore Redis event {event_type} with invalid JSON: {e}");
                continue;
            }
        };
        debug!("Received event {event_type} for cluster {cluster_id:?}");
        let _ = events.send(StoreMessage::Event(Arc::new(StoreEvent {
            event_type: event_type.to_string(),
            cluster_id,
            data_json: data_json.to_string(),
        })));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn screen_token_binds_to_first_ip() {
        let store = Store::memory();
        let token = store.create_screen_token().await.unwrap();
        assert_eq!(token.len(), SCREEN_TOKEN_LENGTH);
        store.redeem_screen_token(&token, "10.0.0.1").await.unwrap();
        store.redeem_screen_token(&token, "10.0.0.1").await.unwrap();
        assert!(store.redeem_screen_token(&token, "10.0.0.2").await.is_err());
        assert!(
            store
                .redeem_screen_token("unknown", "10.0.0.1")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn memory_store_roundtrip_and_events() {
        let store = Store::memory();
        let mut rx = store.subscribe();
        store
            .publish(
                "clusterdelta",
                &serde_json::json!({"cluster_id": "c1", "delta": []}),
            )
            .await
            .unwrap();
        match rx.recv().await.unwrap() {
            StoreMessage::Event(e) => {
                assert_eq!(e.event_type, "clusterdelta");
                assert_eq!(e.cluster_id.as_deref(), Some("c1"));
            }
            StoreMessage::Resync => panic!("unexpected resync"),
        }
        let status = ClusterStatus {
            last_query_time: Some(1.5),
            backoff: None,
        };
        store.set_cluster_status("c1", &status).await.unwrap();
        assert_eq!(store.get_cluster_status("c1").await.unwrap(), status);
        assert_eq!(
            store.get_cluster_status("c2").await.unwrap(),
            ClusterStatus::default()
        );
    }
}
