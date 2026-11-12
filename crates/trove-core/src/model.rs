//! Domain model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok($name::$variant),)+
                    other => Err(Error::bad_request(format!(
                        concat!("unknown ", stringify!($name), " '{}'"), other))),
                }
            }
        }
    };
}

string_enum!(
    /// Package ecosystem served by a repository.
    Format {
        Raw => "raw",
        Maven => "maven",
        Npm => "npm",
        Pypi => "pypi",
        Cargo => "cargo",
        Docker => "docker",
        Apt => "apt",
        Yum => "yum",
        Git => "git",
        Skills => "skills",
    }
);

impl Format {
    /// Repository kinds implemented for this format.
    /// Formats whose groups can forward uploads to a hosted member.
    pub fn supports_group_deployment(&self) -> bool {
        matches!(self, Format::Raw | Format::Maven | Format::Npm | Format::Pypi | Format::Cargo)
    }

    pub fn supported_kinds(&self) -> &'static [RepoKind] {
        match self {
            Format::Apt | Format::Git | Format::Skills => &[RepoKind::Proxy],
            Format::Docker | Format::Yum => &[RepoKind::Proxy, RepoKind::Group],
            _ => &[RepoKind::Hosted, RepoKind::Proxy, RepoKind::Group],
        }
    }
}

string_enum!(
    /// How a repository obtains its content (mirrors Nexus' recipe types).
    RepoKind {
        Hosted => "hosted",
        Proxy => "proxy",
        Group => "group",
    }
);

string_enum!(
    /// Built-in roles. Permissions are cumulative: admin ⊃ deployer ⊃ viewer.
    Role {
        Viewer => "viewer",
        Deployer => "deployer",
        Admin => "admin",
    }
);

impl Role {
    pub fn can_read(&self) -> bool {
        true
    }
    pub fn can_deploy(&self) -> bool {
        matches!(self, Role::Deployer | Role::Admin)
    }
    pub fn is_admin(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// Whether artifacts in a hosted repository may be overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicy {
    /// Overwrites allowed.
    AllowRedeploy,
    /// Each path may be written once (default; matches release semantics).
    #[default]
    AllowOnce,
    /// No writes at all.
    ReadOnly,
}

/// Maven version policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VersionPolicy {
    #[default]
    Release,
    Snapshot,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ProxyConfig {
    /// Upstream base URL, e.g. `https://repo1.maven.org/maven2/`.
    pub remote_url: String,
    /// How long cached *metadata* (indexes, packuments, maven-metadata.xml) is
    /// considered fresh, in seconds. Content files are immutable and cached forever.
    pub metadata_max_age_secs: u64,
    /// Optional upstream basic-auth credentials.
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// Never contact upstream; serve only what is cached.
    pub offline: bool,
    /// Cached metadata older than this timestamp (ms) is considered stale.
    /// Set by "invalidate cache".
    pub invalidated_at: i64,
    /// How this repository reaches its remote.
    pub outbound: OutboundProxy,
    /// When non-zero, cached *content* is also revalidated after this many
    /// seconds (conditional request, so unchanged files are not downloaded
    /// again). For raw proxies of mutable files, e.g. a marketplace index on a
    /// branch. Zero keeps the default: content is immutable.
    pub content_max_age_secs: u64,
}

/// Outbound proxy choice for a proxy repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutboundMode {
    /// Use the server-wide setting (`http_client.proxy` or `HTTPS_PROXY` & co.).
    #[default]
    Inherit,
    /// Connect directly, ignoring any configured or environment proxy.
    Direct,
    /// Use the proxy given in `url`.
    Custom,
}

pub const OUTBOUND_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct OutboundProxy {
    pub mode: OutboundMode,
    /// `http://host:3128`, `https://host:443`, `socks5://host:1080` (local DNS)
    /// or `socks5h://host:1080` (the proxy resolves host names).
    pub url: String,
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// Comma-separated hosts, domains (`.corp.example`) or CIDRs that bypass the proxy.
    pub no_proxy: String,
}

impl OutboundProxy {
    pub fn validate(&self) -> Result<(), Error> {
        if self.mode != OutboundMode::Custom {
            return Ok(());
        }
        let parsed = url::Url::parse(self.url.trim())
            .map_err(|e| Error::bad_request(format!("invalid outbound proxy URL '{}': {e}", self.url)))?;
        if !OUTBOUND_SCHEMES.contains(&parsed.scheme()) {
            return Err(Error::bad_request(format!(
                "outbound proxy must use one of {}: got '{}'",
                OUTBOUND_SCHEMES.join(", "),
                parsed.scheme()
            )));
        }
        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(Error::bad_request("outbound proxy URL needs a host"));
        }
        if parsed.port_or_known_default().is_none() {
            return Err(Error::bad_request("outbound proxy URL needs a port, e.g. socks5://proxy:1080"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct GroupConfig {
    /// Ordered list of member repository names. The first match wins for
    /// content; metadata is merged where the format supports it.
    pub members: Vec<String>,
    /// Hosted member that receives uploads sent to the group ("group
    /// deployment"), so one URL serves both downloads and publishing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writable_member: Option<String>,
}

/// Per-repository settings stored as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct RepoConfig {
    pub write_policy: WritePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_policy: Option<VersionPolicy>,
    /// Only signed-in users may read (even when anonymous read is on).
    pub private: bool,
    /// Raw repositories: how browsers should treat downloaded files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_disposition: Option<ContentDisposition>,
}

/// `Content-Disposition` for raw downloads (Nexus' "Content Disposition").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContentDisposition {
    /// Always download as a file (safe default).
    #[default]
    Attachment,
    /// Let browsers display the file; served in a sandbox so uploaded HTML
    /// cannot run scripts on Trove's origin.
    Inline,
}

pub const DEFAULT_METADATA_MAX_AGE_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub format: Format,
    pub kind: RepoKind,
    pub online: bool,
    pub description: String,
    pub config: RepoConfig,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Repository {
    pub fn proxy(&self) -> Option<&ProxyConfig> {
        self.config.proxy.as_ref()
    }
    pub fn members(&self) -> &[String] {
        self.config.group.as_ref().map(|g| g.members.as_slice()).unwrap_or(&[])
    }

    /// The member that receives writes sent to this group, if any.
    pub fn writable_member(&self) -> Option<&str> {
        self.config.group.as_ref().and_then(|g| g.writable_member.as_deref()).filter(|m| !m.is_empty())
    }

    /// Validate structural invariants of a repository definition.
    pub fn validate(&self) -> Result<(), Error> {
        crate::util::validate_name(&self.name)?;
        if !self.format.supported_kinds().contains(&self.kind) {
            return Err(Error::bad_request(format!(
                "{} repositories do not support the {} type yet",
                self.format, self.kind
            )));
        }
        match self.kind {
            RepoKind::Hosted => {}
            RepoKind::Proxy => {
                let p = self
                    .config
                    .proxy
                    .as_ref()
                    .ok_or_else(|| Error::bad_request("proxy repositories need proxy.remote_url"))?;
                if !(p.remote_url.starts_with("http://") || p.remote_url.starts_with("https://")) {
                    return Err(Error::bad_request("proxy.remote_url must be an http(s) URL"));
                }
                p.outbound.validate()?;
            }
            RepoKind::Group => {
                let g = self
                    .config
                    .group
                    .as_ref()
                    .ok_or_else(|| Error::bad_request("group repositories need group.members"))?;
                if g.members.iter().any(|m| m == &self.name) {
                    return Err(Error::bad_request("a group cannot contain itself"));
                }
            }
        }
        Ok(())
    }
}

/// Content-addressed blob metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobInfo {
    pub sha256: String,
    pub sha1: String,
    pub md5: String,
    pub sha512: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    pub repo_id: String,
    pub path: String,
    pub content_type: String,
    pub blob: BlobInfo,
    pub uploader: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_downloaded_at: Option<i64>,
    /// Format-specific attributes (e.g. upstream URL, cache timestamps).
    pub attributes: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub role: Role,
    pub disabled: bool,
    pub created_at: i64,
    #[serde(skip)]
    pub password_hash: String,
}

string_enum!(
    TokenKind {
        Session => "session",
        Api => "api",
    }
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub kind: TokenKind,
    /// First characters of the secret, for display only.
    pub prefix: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub ts: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub client_ip: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_roundtrip() {
        for f in Format::ALL {
            assert_eq!(f.as_str().parse::<Format>().unwrap(), *f);
        }
        assert!("helm".parse::<Format>().is_err());
        assert!(Format::Apt.supported_kinds() == [RepoKind::Proxy]);
        assert_eq!(serde_json::to_string(&RepoKind::Proxy).unwrap(), "\"proxy\"");
    }

    #[test]
    fn repo_validation() {
        let mut r = Repository {
            id: "1".into(),
            name: "maven-central".into(),
            format: Format::Maven,
            kind: RepoKind::Proxy,
            online: true,
            description: String::new(),
            config: RepoConfig::default(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(r.validate().is_err());
        r.config.proxy =
            Some(ProxyConfig { remote_url: "https://repo1.maven.org/maven2/".into(), ..Default::default() });
        assert!(r.validate().is_ok());
        let p = r.config.proxy.as_mut().unwrap();
        p.outbound = OutboundProxy { mode: OutboundMode::Custom, url: "ftp://x:21".into(), ..Default::default() };
        assert!(r.validate().is_err());
        let p = r.config.proxy.as_mut().unwrap();
        p.outbound.url = "socks5h://10.0.0.1".into();
        assert!(r.validate().is_err(), "socks without port");
        let p = r.config.proxy.as_mut().unwrap();
        p.outbound.url = "socks5h://10.0.0.1:1080".into();
        assert!(r.validate().is_ok());
        let p = r.config.proxy.as_mut().unwrap();
        p.outbound.url = "http://squid.corp:3128".into();
        assert!(r.validate().is_ok());
        r.name = "bad name".into();
        assert!(r.validate().is_err());
    }
}
