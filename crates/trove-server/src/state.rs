use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::{Mutex, RwLock};
use trove_core::config::Config;
use trove_core::model::Repository;
use trove_core::{Error, Result};
use trove_db::Db;
use trove_storage::BlobStore;

use crate::auth::CredentialCache;

pub const SETTING_ANONYMOUS_READ: &str = "anonymous_read";

/// Process-wide counters exported at `/metrics`.
#[derive(Default)]
pub struct Metrics {
    pub downloads: AtomicU64,
    pub uploads: AtomicU64,
    pub proxy_hits: AtomicU64,
    pub proxy_fetches: AtomicU64,
    pub proxy_not_modified: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub auth_failures: AtomicU64,
}

impl Metrics {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct Inner {
    pub cfg: Config,
    pub db: Db,
    pub blobs: BlobStore,
    pub http: reqwest::Client,
    pub anonymous_read: AtomicBool,
    pub metrics: Metrics,
    pub credentials: CredentialCache,
    /// Uploads hold a read lock; blob GC takes the write lock so it never
    /// races with a write that is about to reference a deduplicated blob.
    pub gc_lock: RwLock<()>,
    /// Per-key locks so concurrent requests for the same uncached upstream
    /// file trigger a single download.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Short-lived cache of upstream 404s.
    negative: Mutex<HashMap<String, Instant>>,
    pub started_at: Instant,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

const NEGATIVE_TTL: Duration = Duration::from_secs(60);

impl AppState {
    pub async fn new(cfg: Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.server.data_dir)
            .with_context(|| format!("creating data dir {}", cfg.server.data_dir.display()))?;
        let db = Db::connect(&cfg.database_url(), cfg.database.max_connections).await?;
        let blobs = BlobStore::from_config(&cfg)?;
        let http = build_http_client(&cfg)?;
        let anonymous = match db.get_setting(SETTING_ANONYMOUS_READ).await? {
            Some(v) => v == "true",
            None => cfg.security.anonymous_read,
        };
        Ok(Self(Arc::new(Inner {
            cfg,
            db,
            blobs,
            http,
            anonymous_read: AtomicBool::new(anonymous),
            metrics: Metrics::default(),
            credentials: CredentialCache::default(),
            gc_lock: RwLock::new(()),
            inflight: Mutex::new(HashMap::new()),
            negative: Mutex::new(HashMap::new()),
            started_at: Instant::now(),
        })))
    }

    pub fn anonymous_read(&self) -> bool {
        self.anonymous_read.load(Ordering::Relaxed)
    }

    pub fn max_upload(&self) -> u64 {
        self.cfg.server.max_upload_mb.saturating_mul(1024 * 1024)
    }

    pub async fn repo(&self, name: &str) -> Result<Repository> {
        self.db.get_repo_by_name(name).await?.ok_or_else(|| Error::not_found(format!("repository '{name}'")))
    }

    /// Acquire the single-flight lock for `key`.
    pub async fn flight_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.inflight.lock().await;
        if map.len() > 10_000 {
            map.retain(|_, v| Arc::strong_count(v) > 1);
        }
        map.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    pub async fn is_negative(&self, key: &str) -> bool {
        let mut map = self.negative.lock().await;
        match map.get(key) {
            Some(t) if t.elapsed() < NEGATIVE_TTL => true,
            Some(_) => {
                map.remove(key);
                false
            }
            None => false,
        }
    }

    pub async fn set_negative(&self, key: &str) {
        let mut map = self.negative.lock().await;
        if map.len() > 50_000 {
            map.retain(|_, t| t.elapsed() < NEGATIVE_TTL);
        }
        map.insert(key.to_string(), Instant::now());
    }

    pub async fn clear_negative_for_repo(&self, repo_id: &str) {
        let prefix = format!("{repo_id}:");
        self.negative.lock().await.retain(|k, _| !k.starts_with(&prefix));
    }
}

fn build_http_client(cfg: &Config) -> anyhow::Result<reqwest::Client> {
    let hc = &cfg.http_client;
    let mut builder = reqwest::Client::builder()
        .user_agent(hc.user_agent.clone())
        .timeout(Duration::from_secs(hc.timeout_secs))
        .connect_timeout(Duration::from_secs(hc.connect_timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(10));
    if !hc.proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(&hc.proxy).context("invalid http_client.proxy")?);
    }
    if !hc.ca_bundle.is_empty() {
        let pem =
            std::fs::read(&hc.ca_bundle).with_context(|| format!("reading http_client.ca_bundle {}", hc.ca_bundle))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).context("parsing CA bundle")?;
        builder = builder.tls_certs_merge(certs);
    }
    Ok(builder.build()?)
}
