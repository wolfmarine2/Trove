//! Maven 2 repository layout (`mvn deploy`, Gradle, sbt, Ivy in m2 mode).
//!
//! * Checksums (`.sha1`, `.md5`, `.sha256`, `.sha512`) are generated on the
//!   fly when not stored explicitly.
//! * Group repositories merge `maven-metadata.xml` across members.
//! * Version policy (release / snapshot / mixed) is enforced on deploy.

use std::cmp::Ordering;

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use md5::Digest;
use trove_core::model::{Asset, RepoKind, VersionPolicy};
use trove_core::util::normalize_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::audit;
use crate::content::{self, StoreOpts};

const CHECKSUMS: &[&str] = &[".sha1", ".md5", ".sha256", ".sha512"];

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn checksum_suffix(path: &str) -> Option<&'static str> {
    CHECKSUMS.iter().copied().find(|s| path.ends_with(s))
}

pub fn is_metadata(path: &str) -> bool {
    let name = file_name(path);
    name.starts_with("maven-metadata.xml") || name.starts_with("archetype-catalog.xml")
}

fn is_snapshot_path(path: &str) -> bool {
    path.split('/').any(|seg| seg.ends_with("-SNAPSHOT"))
}

fn checksum_of(asset: &Asset, suffix: &str) -> String {
    match suffix {
        ".sha1" => asset.blob.sha1.clone(),
        ".md5" => asset.blob.md5.clone(),
        ".sha256" => asset.blob.sha256.clone(),
        _ => asset.blob.sha512.clone(),
    }
}

fn checksum_bytes(data: &[u8], suffix: &str) -> String {
    match suffix {
        ".sha1" => hex::encode(sha1_digest(data)),
        ".md5" => hex::encode(md5::Md5::digest(data)),
        ".sha256" => hex::encode(sha2::Sha256::digest(data)),
        _ => hex::encode(sha2::Sha512::digest(data)),
    }
}

fn sha1_digest(data: &[u8]) -> Vec<u8> {
    use sha1::Sha1;
    Sha1::digest(data).to_vec()
}

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let wants_dir = ctx.path.is_empty() || ctx.path.ends_with('/');
    let path = normalize_path(&ctx.path)?;
    match ctx.method.clone() {
        Method::GET | Method::HEAD => {
            ctx.principal.require_read(&ctx.state)?;
            if wants_dir {
                return Err(Error::not_found("browse directories in the web UI"));
            }
            get(&ctx, &path).await
        }
        Method::PUT => {
            ctx.principal.require_deploy()?;
            if path.is_empty() || wants_dir {
                return Err(Error::bad_request("a file path is required"));
            }
            enforce_version_policy(&ctx, &path)?;
            let meta = is_metadata(&path);
            content::check_writable(&ctx.state, &ctx.repo, &path, meta).await?;
            let body = ctx.body_stream();
            let opts = StoreOpts { is_metadata: meta, ..Default::default() };
            let asset = content::store(&ctx.state, &ctx.repo, &path, body, &ctx.principal, opts).await?;
            if !meta && checksum_suffix(&path).is_none() {
                audit::record(
                    &ctx.state,
                    &ctx.principal,
                    &ctx.headers,
                    "asset.upload",
                    format!("{}/{}", ctx.repo.name, path),
                    format!("{} bytes", asset.blob.size),
                )
                .await;
            }
            Ok(StatusCode::CREATED.into_response())
        }
        Method::DELETE => super::raw::delete_path(&ctx, &path).await,
        other => Err(Error::MethodNotAllowed(other.to_string())),
    }
}

fn enforce_version_policy(ctx: &Ctx, path: &str) -> Result<()> {
    if is_metadata(path) {
        return Ok(());
    }
    let snapshot = is_snapshot_path(path);
    match ctx.repo.config.version_policy.unwrap_or_default() {
        VersionPolicy::Release if snapshot => {
            Err(Error::bad_request(format!("repository '{}' does not accept SNAPSHOT versions", ctx.repo.name)))
        }
        VersionPolicy::Snapshot if !snapshot => {
            Err(Error::bad_request(format!("repository '{}' only accepts SNAPSHOT versions", ctx.repo.name)))
        }
        _ => Ok(()),
    }
}

async fn get(ctx: &Ctx, path: &str) -> Result<Response> {
    let (state, repo) = (&ctx.state, &ctx.repo);
    let suffix = checksum_suffix(path);
    let base = suffix.map(|s| &path[..path.len() - s.len()]).unwrap_or(path);

    if repo.kind == RepoKind::Group && file_name(base) == "maven-metadata.xml" {
        if let Some(merged) = merged_metadata(ctx, base).await? {
            return Ok(match suffix {
                Some(s) => content::serve_bytes("text/plain", checksum_bytes(&merged, s), &ctx.method, &ctx.headers),
                None => content::serve_bytes("application/xml", merged, &ctx.method, &ctx.headers),
            });
        }
        return Err(Error::not_found(path.to_string()));
    }

    let meta = is_metadata(path);
    let direct = content::resolve(state, repo, path, meta, 0).await;
    if let Ok(Some(asset)) = &direct {
        return content::serve_asset(state, asset, &ctx.method, &ctx.headers).await;
    }
    if let Some(s) = suffix {
        if let Some(asset) = content::resolve(state, repo, base, is_metadata(base), 0).await? {
            return Ok(content::serve_bytes("text/plain", checksum_of(&asset, s), &ctx.method, &ctx.headers));
        }
    }
    direct?;
    Err(Error::not_found(path.to_string()))
}

/// Merge `maven-metadata.xml` from every group member that has one.
async fn merged_metadata(ctx: &Ctx, path: &str) -> Result<Option<Bytes>> {
    let mut docs = Vec::new();
    let mut failure = None;
    for member in content::group_members(&ctx.state, &ctx.repo, 0).await? {
        match content::resolve(&ctx.state, &member, path, true, 1).await {
            Ok(Some(asset)) => docs.push(content::read_asset(&ctx.state, &asset).await?),
            Ok(None) => {}
            Err(e) => failure = Some(content::member_failed(&member, e)),
        }
    }
    if docs.is_empty() {
        return failure.map_or(Ok(None), Err);
    }
    if docs.len() == 1 {
        return Ok(docs.pop());
    }
    let parsed: Vec<Metadata> = docs.iter().filter_map(|d| Metadata::parse(&String::from_utf8_lossy(d))).collect();
    // Snapshot (GAV-level) or plugin metadata cannot be merged meaningfully: first wins.
    if parsed.len() != docs.len() {
        return Ok(docs.into_iter().next());
    }
    Ok(Some(Bytes::from(Metadata::merge(&parsed).render())))
}

/// GA-level `maven-metadata.xml`.
#[derive(Debug, Default, Clone)]
pub struct Metadata {
    pub group_id: String,
    pub artifact_id: String,
    pub versions: Vec<String>,
    pub last_updated: String,
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

impl Metadata {
    pub fn parse(xml: &str) -> Option<Self> {
        let versioning = tag(xml, "versioning")?;
        // GAV-level (snapshot) metadata has a top-level <version> and a <snapshot>.
        if versioning.contains("<snapshot>") || xml.contains("<plugins>") {
            return None;
        }
        let versions_block = tag(versioning, "versions")?;
        let mut versions = Vec::new();
        let mut rest = versions_block;
        while let Some(v) = tag(rest, "version") {
            versions.push(v.to_string());
            let idx = rest.find("</version>")? + "</version>".len();
            rest = &rest[idx..];
        }
        Some(Self {
            group_id: tag(xml, "groupId").unwrap_or_default().to_string(),
            artifact_id: tag(xml, "artifactId").unwrap_or_default().to_string(),
            versions,
            last_updated: tag(versioning, "lastUpdated").unwrap_or_default().to_string(),
        })
    }

    pub fn merge(docs: &[Metadata]) -> Self {
        let mut versions: Vec<String> = Vec::new();
        for d in docs {
            for v in &d.versions {
                if !versions.contains(v) {
                    versions.push(v.clone());
                }
            }
        }
        versions.sort_by(|a, b| compare_versions(a, b));
        Self {
            group_id: docs[0].group_id.clone(),
            artifact_id: docs[0].artifact_id.clone(),
            versions,
            last_updated: docs.iter().map(|d| d.last_updated.clone()).max().unwrap_or_default(),
        }
    }

    pub fn latest(&self) -> Option<&String> {
        self.versions.iter().max_by(|a, b| compare_versions(a, b))
    }

    pub fn release(&self) -> Option<&String> {
        self.versions.iter().filter(|v| !v.ends_with("-SNAPSHOT")).max_by(|a, b| compare_versions(a, b))
    }

    pub fn render(&self) -> String {
        let esc = super::raw::escape;
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");
        out.push_str(&format!(
            "  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n",
            esc(&self.group_id),
            esc(&self.artifact_id)
        ));
        out.push_str("  <versioning>\n");
        if let Some(l) = self.latest() {
            out.push_str(&format!("    <latest>{}</latest>\n", esc(l)));
        }
        if let Some(r) = self.release() {
            out.push_str(&format!("    <release>{}</release>\n", esc(r)));
        }
        out.push_str("    <versions>\n");
        for v in &self.versions {
            out.push_str(&format!("      <version>{}</version>\n", esc(v)));
        }
        out.push_str("    </versions>\n");
        out.push_str(&format!("    <lastUpdated>{}</lastUpdated>\n", esc(&self.last_updated)));
        out.push_str("  </versioning>\n</metadata>\n");
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Item {
    Num(u64),
    Qual(String),
}

fn qualifier_rank(q: &str) -> (u8, &str) {
    match q {
        "alpha" | "a" => (1, ""),
        "beta" | "b" => (2, ""),
        "milestone" | "m" => (3, ""),
        "rc" | "cr" => (4, ""),
        "snapshot" => (5, ""),
        "" | "ga" | "final" | "release" => (6, ""),
        "sp" => (7, ""),
        other => (8, other),
    }
}

fn tokenize(v: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut cur_digit = None;
    let flush = |cur: &mut String, items: &mut Vec<Item>, digit: Option<bool>| {
        if cur.is_empty() {
            return;
        }
        if digit == Some(true) {
            items.push(Item::Num(cur.parse().unwrap_or(u64::MAX)));
        } else {
            items.push(Item::Qual(cur.to_ascii_lowercase()));
        }
        cur.clear();
    };
    for c in v.chars() {
        if c == '.' || c == '-' || c == '_' {
            flush(&mut cur, &mut items, cur_digit);
            cur_digit = None;
            continue;
        }
        let d = c.is_ascii_digit();
        if cur_digit.is_some_and(|cd| cd != d) {
            flush(&mut cur, &mut items, cur_digit);
        }
        cur_digit = Some(d);
        cur.push(c);
    }
    flush(&mut cur, &mut items, cur_digit);
    items
}

/// Approximation of Maven's `ComparableVersion` ordering.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (ta, tb) = (tokenize(a), tokenize(b));
    for i in 0..ta.len().max(tb.len()) {
        let ord = match (ta.get(i), tb.get(i)) {
            (Some(Item::Num(x)), Some(Item::Num(y))) => x.cmp(y),
            (Some(Item::Num(x)), None) => x.cmp(&0),
            (None, Some(Item::Num(y))) => 0.cmp(y),
            (Some(Item::Num(_)), Some(Item::Qual(_))) => Ordering::Greater,
            (Some(Item::Qual(_)), Some(Item::Num(_))) => Ordering::Less,
            (Some(Item::Qual(x)), Some(Item::Qual(y))) => qualifier_rank(x).cmp(&qualifier_rank(y)),
            (Some(Item::Qual(x)), None) => qualifier_rank(x).cmp(&qualifier_rank("")),
            (None, Some(Item::Qual(y))) => qualifier_rank("").cmp(&qualifier_rank(y)),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        let mut v = vec!["1.10", "1.2", "1.2-rc1", "1.2-SNAPSHOT", "1.2.1", "1.0-alpha", "1.0-beta2", "1.0"];
        v.sort_by(|a, b| compare_versions(a, b));
        assert_eq!(v, vec!["1.0-alpha", "1.0-beta2", "1.0", "1.2-rc1", "1.2-SNAPSHOT", "1.2", "1.2.1", "1.10"]);
    }

    #[test]
    fn metadata_merge() {
        let a = Metadata::parse(
            "<metadata><groupId>g</groupId><artifactId>a</artifactId><versioning><latest>1.1</latest>\
             <versions><version>1.0</version><version>1.1</version></versions><lastUpdated>20240101000000</lastUpdated></versioning></metadata>",
        )
        .unwrap();
        let b = Metadata::parse(
            "<metadata><groupId>g</groupId><artifactId>a</artifactId><versioning>\
             <versions><version>1.1</version><version>2.0-SNAPSHOT</version><version>1.5</version></versions>\
             <lastUpdated>20250101000000</lastUpdated></versioning></metadata>",
        )
        .unwrap();
        let m = Metadata::merge(&[a, b]);
        assert_eq!(m.versions, vec!["1.0", "1.1", "1.5", "2.0-SNAPSHOT"]);
        assert_eq!(m.latest().unwrap(), "2.0-SNAPSHOT");
        assert_eq!(m.release().unwrap(), "1.5");
        let xml = m.render();
        assert!(xml.contains("<lastUpdated>20250101000000</lastUpdated>"));
        assert!(Metadata::parse(&xml).is_some());
    }

    #[test]
    fn snapshot_metadata_not_merged() {
        let xml = "<metadata><version>1.0-SNAPSHOT</version><versioning><snapshot><timestamp>x</timestamp></snapshot></versioning></metadata>";
        assert!(Metadata::parse(xml).is_none());
    }

    #[test]
    fn classification() {
        assert!(is_metadata("org/a/maven-metadata.xml.sha1"));
        assert!(!is_metadata("org/a/1.0/a-1.0.jar"));
        assert!(is_snapshot_path("org/a/1.0-SNAPSHOT/a-1.0-20240101.000000-1.jar"));
        assert_eq!(checksum_suffix("a.jar.sha256"), Some(".sha256"));
    }
}
