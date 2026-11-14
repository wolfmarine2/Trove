use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::{Mutex, RwLock};
use trove_core::config::{Config, TlsBackend};
use trove_core::model::{OutboundMode, OutboundProxy, Repository};
use trove_core::{Error, Result};
use trove_db::Db;
use trove_storage::BlobStore;

use crate::auth::CredentialCache;

pub const SETTING_ANONYMOUS_READ: &str = "anonymous_read";
/// Docker repository serving `/v2/<image>` when the first segment is not a repository name.
pub const SETTING_DOCKER_DEFAULT_REPO: &str = "docker_default_repository";

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
    /// HTTP clients for repositories with their own outbound proxy, keyed by
    /// the proxy settings so edits take effect immediately.
    clients: std::sync::Mutex<HashMap<String, reqwest::Client>>,
    /// Upstream registry bearer tokens: key → (token, expiry).
    registry_tokens: Mutex<HashMap<String, (String, Instant)>>,
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
        let http = build_http_client(&cfg, default_choice(&cfg))?;
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
            registry_tokens: Mutex::new(HashMap::new()),
            clients: std::sync::Mutex::new(HashMap::new()),
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

    /// HTTP client to use for a repository's upstream requests.
    pub fn client_for(&self, repo: &Repository) -> Result<reqwest::Client> {
        match repo.proxy() {
            Some(p) => self.client_for_outbound(&p.outbound),
            None => Ok(self.http.clone()),
        }
    }

    pub fn client_for_outbound(&self, o: &OutboundProxy) -> Result<reqwest::Client> {
        let choice = match o.mode {
            OutboundMode::Inherit => return Ok(self.http.clone()),
            OutboundMode::Direct => ProxyChoice::Direct,
            OutboundMode::Custom => {
                o.validate()?;
                ProxyChoice::Url { url: &o.url, username: &o.username, password: &o.password, no_proxy: &o.no_proxy }
            }
        };
        let key = format!(
            "{:?}|{}|{}|{}|{}",
            o.mode,
            o.url.trim(),
            o.username,
            trove_core::util::sha256_hex(&o.password),
            o.no_proxy
        );
        let mut clients = self.clients.lock().map_err(|_| Error::internal("client cache poisoned"))?;
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let client = build_http_client(&self.cfg, choice).map_err(|e| Error::bad_request(format!("{e:#}")))?;
        if clients.len() > 256 {
            clients.clear();
        }
        clients.insert(key, client.clone());
        Ok(client)
    }

    pub async fn cached_registry_token(&self, key: &str) -> Option<String> {
        let map = self.registry_tokens.lock().await;
        map.get(key).filter(|(_, exp)| *exp > Instant::now()).map(|(t, _)| t.clone())
    }

    pub async fn cache_registry_token(&self, key: &str, token: &str, ttl: Duration) {
        let mut map = self.registry_tokens.lock().await;
        if map.len() > 10_000 {
            let now = Instant::now();
            map.retain(|_, (_, exp)| *exp > now);
        }
        map.insert(key.to_string(), (token.to_string(), Instant::now() + ttl));
    }

    pub async fn clear_negative_for_repo(&self, repo_id: &str) {
        let prefix = format!("{repo_id}:");
        self.negative.lock().await.retain(|k, _| !k.starts_with(&prefix));
    }
}

/// Which proxy an HTTP client should use.
pub enum ProxyChoice<'a> {
    /// reqwest defaults: `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`.
    Environment,
    /// No proxy at all.
    Direct,
    /// An explicit proxy.
    Url { url: &'a str, username: &'a str, password: &'a str, no_proxy: &'a str },
}

/// Build a `reqwest::Proxy` for http(s) or socks5(h) URLs. Credentials go into
/// the URL for SOCKS (RFC 1929) and into `Proxy-Authorization` for HTTP.
pub fn make_proxy(url: &str, username: &str, password: &str, no_proxy: &str) -> anyhow::Result<reqwest::Proxy> {
    let mut parsed = url::Url::parse(url.trim()).with_context(|| format!("invalid proxy URL {url}"))?;
    let socks = parsed.scheme().starts_with("socks");
    if socks && !username.is_empty() {
        parsed.set_username(username).map_err(|_| anyhow::anyhow!("cannot set proxy username"))?;
        parsed.set_password(Some(password)).map_err(|_| anyhow::anyhow!("cannot set proxy password"))?;
    }
    let mut proxy = reqwest::Proxy::all(parsed.as_str()).with_context(|| format!("unsupported proxy URL {url}"))?;
    if !socks && !username.is_empty() {
        proxy = proxy.basic_auth(username, password);
    }
    if !no_proxy.trim().is_empty() {
        proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
    }
    Ok(proxy)
}

pub fn build_http_client(cfg: &Config, choice: ProxyChoice<'_>) -> anyhow::Result<reqwest::Client> {
    let hc = &cfg.http_client;
    let mut builder = reqwest::Client::builder()
        .user_agent(hc.user_agent.clone())
        // Idle timeout per read: large artifacts may legitimately take long overall.
        .read_timeout(Duration::from_secs(hc.timeout_secs))
        .connect_timeout(Duration::from_secs(hc.connect_timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(10));
    builder = match hc.tls_backend {
        TlsBackend::Native => builder.tls_backend_native(),
        TlsBackend::Rustls => builder.tls_backend_rustls(),
    };
    builder = match choice {
        ProxyChoice::Environment => builder,
        ProxyChoice::Direct => builder.no_proxy(),
        ProxyChoice::Url { url, username, password, no_proxy } => {
            builder.proxy(make_proxy(url, username, password, no_proxy)?)
        }
    };
    if !hc.ca_bundle.is_empty() {
        let pem =
            std::fs::read(&hc.ca_bundle).with_context(|| format!("reading http_client.ca_bundle {}", hc.ca_bundle))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).context("parsing CA bundle")?;
        builder = builder.tls_certs_merge(certs);
    }
    Ok(builder.build()?)
}

fn default_choice(cfg: &Config) -> ProxyChoice<'_> {
    let hc = &cfg.http_client;
    if hc.proxy.is_empty() {
        ProxyChoice::Environment
    } else {
        ProxyChoice::Url { url: &hc.proxy, username: "", password: "", no_proxy: &hc.no_proxy }
    }
}
