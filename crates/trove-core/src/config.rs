//! Server configuration.
//!
//! Configuration is read from a TOML file (default `trove.toml`) and can be
//! overridden with environment variables of the form `TROVE__SECTION__KEY`,
//! e.g. `TROVE__DATABASE__URL=postgres://...`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub security: SecurityConfig,
    pub http_client: HttpClientConfig,
    pub ui: UiConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Socket address to listen on.
    pub bind: String,
    /// Externally visible base URL, used when Trove must emit absolute URLs
    /// (npm tarball links, Cargo `config.json`, PyPI file links).
    /// When empty, it is derived from the request `Host` header.
    pub base_url: String,
    /// Maximum accepted upload size in MiB.
    pub max_upload_mb: u64,
    /// Data directory for the default SQLite database and filesystem blob store.
    pub data_dir: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8081".into(),
            base_url: String::new(),
            max_upload_mb: 4096,
            data_dir: PathBuf::from("data"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `sqlite://path?mode=rwc` or `postgres://user:pass@host/db`.
    /// Empty means `sqlite://<data_dir>/trove.db?mode=rwc`.
    pub url: String,
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { url: String::new(), max_connections: 16 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    #[default]
    Filesystem,
    S3,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub kind: StorageKind,
    /// Filesystem root. Empty means `<data_dir>/blobs`.
    pub path: String,
    pub s3: S3Config,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    /// Custom endpoint for S3-compatible stores (MinIO, Ceph RGW).
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Prefix inside the bucket.
    pub prefix: String,
    /// Allow plain HTTP endpoints (typical for in-cluster MinIO).
    pub allow_http: bool,
    /// Use path-style addressing instead of virtual-hosted style.
    pub path_style: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Password for the bootstrap `admin` user. If empty, a random password is
    /// generated on first start and written to `<data_dir>/admin.password`.
    pub initial_admin_password: String,
    /// Initial value of the "anonymous read" setting (can be changed in the UI).
    pub anonymous_read: bool,
    /// Lifetime of UI sessions in hours.
    pub session_ttl_hours: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self { initial_admin_password: String::new(), anonymous_read: true, session_ttl_hours: 12 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpClientConfig {
    /// Total request timeout for upstream fetches, in seconds.
    pub timeout_secs: u64,
    /// Connect timeout, in seconds.
    pub connect_timeout_secs: u64,
    /// Outbound HTTP(S) proxy. When empty, `HTTPS_PROXY`/`HTTP_PROXY` are honoured.
    pub proxy: String,
    /// Additional PEM CA bundle to trust (corporate TLS inspection, internal CAs).
    pub ca_bundle: String,
    pub user_agent: String,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 300,
            connect_timeout_secs: 15,
            proxy: String::new(),
            ca_bundle: String::new(),
            user_agent: format!("Trove/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Serve the web UI from this directory instead of the embedded copy.
    pub dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `tracing` env-filter directive, e.g. `info,trove_server=debug`.
    pub level: String,
    /// Emit JSON lines instead of human readable output.
    pub json: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "info".into(), json: false }
    }
}

impl Config {
    /// Load configuration from an optional file plus `TROVE__*` environment overrides.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut root = match path {
            Some(p) if p.exists() => {
                let text =
                    std::fs::read_to_string(p).with_context(|| format!("reading config file {}", p.display()))?;
                text.parse::<toml::Table>().with_context(|| format!("parsing config file {}", p.display()))?
            }
            Some(p) if p != Path::new("trove.toml") => {
                anyhow::bail!("config file {} does not exist", p.display())
            }
            _ => toml::Table::new(),
        };
        apply_env_overrides(&mut root, std::env::vars());
        let cfg: Config = toml::Value::Table(root).try_into().context("invalid configuration")?;
        Ok(cfg)
    }

    pub fn database_url(&self) -> String {
        if self.database.url.is_empty() {
            format!("sqlite://{}?mode=rwc", self.server.data_dir.join("trove.db").display())
        } else {
            self.database.url.clone()
        }
    }

    pub fn storage_path(&self) -> PathBuf {
        if self.storage.path.is_empty() {
            self.server.data_dir.join("blobs")
        } else {
            PathBuf::from(&self.storage.path)
        }
    }
}

fn apply_env_overrides(root: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    for (key, value) in vars {
        let Some(rest) = key.strip_prefix("TROVE__") else { continue };
        let parts: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        if parts.iter().any(|p| p.is_empty()) {
            continue;
        }
        let mut table = &mut *root;
        for section in &parts[..parts.len() - 1] {
            let entry = table.entry(section.clone()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if !entry.is_table() {
                *entry = toml::Value::Table(toml::Table::new());
            }
            table = entry.as_table_mut().expect("just ensured table");
        }
        table.insert(parts[parts.len() - 1].clone(), parse_scalar(&value));
    }
}

fn parse_scalar(v: &str) -> toml::Value {
    if let Ok(b) = v.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = v.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    toml::Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_nested_keys() {
        let mut root = toml::Table::new();
        apply_env_overrides(
            &mut root,
            vec![
                ("TROVE__DATABASE__URL".to_string(), "postgres://x".to_string()),
                ("TROVE__SECURITY__ANONYMOUS_READ".to_string(), "false".to_string()),
                ("TROVE__SERVER__MAX_UPLOAD_MB".to_string(), "10".to_string()),
                ("OTHER".to_string(), "1".to_string()),
            ]
            .into_iter(),
        );
        let cfg: Config = toml::Value::Table(root).try_into().unwrap();
        assert_eq!(cfg.database.url, "postgres://x");
        assert!(!cfg.security.anonymous_read);
        assert_eq!(cfg.server.max_upload_mb, 10);
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert!(cfg.database_url().starts_with("sqlite://"));
        assert_eq!(cfg.storage_path(), PathBuf::from("data/blobs"));
    }
}
