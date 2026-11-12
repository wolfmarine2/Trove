//! YUM / DNF (openEuler, RHEL, Rocky, Alma, CentOS Stream, Fedora, EPEL) proxy.
//!
//! ```ini
//! [openeuler-os]
//! baseurl=http://trove.example.com/repository/openeuler-os/
//! gpgcheck=1
//! ```
//!
//! * `repodata/repomd.xml` (+ `.asc`, `.key`) and other root metadata are
//!   refreshed after the repository's max age.
//! * `repodata/<checksum>-*.xml.gz` and packages are immutable. Repodata files
//!   without a checksum prefix are tied to `repomd.xml` and refetched when it
//!   changes.

use axum::response::Response;
use trove_core::Result;

use super::Ctx;
use super::path_proxy::{self, Policy};

fn has_checksum_prefix(name: &str) -> bool {
    let prefix = name.split('-').next().unwrap_or_default();
    prefix.len() >= 32 && prefix.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn classify(path: &str) -> Policy {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    let in_repodata = dir == "repodata" || dir.ends_with("/repodata");
    if in_repodata {
        if file.starts_with("repomd.xml") {
            return Policy { metadata: true, anchor: None };
        }
        if has_checksum_prefix(file) {
            return Policy { metadata: false, anchor: None };
        }
        return Policy { metadata: true, anchor: Some(format!("{dir}/repomd.xml")) };
    }
    let metadata = matches!(file, ".treeinfo" | "treeinfo" | "media.repo" | "TIME" | "timestamp.txt")
        || file.ends_with(".repo")
        || file.starts_with("RPM-GPG-KEY");
    Policy { metadata, anchor: None }
}

pub async fn handle(ctx: Ctx) -> Result<Response> {
    if ctx.repo.kind == trove_core::model::RepoKind::Group {
        return super::yum_group::handle(ctx).await;
    }
    path_proxy::handle(ctx, classify).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert!(classify("repodata/repomd.xml").metadata);
        assert!(classify("x86_64/os/repodata/repomd.xml.asc").metadata);
        let p = classify("repodata/0123456789abcdef0123456789abcdef-primary.xml.gz");
        assert!(!p.metadata);
        let p = classify("os/repodata/primary.xml.gz");
        assert!(p.metadata);
        assert_eq!(p.anchor.as_deref(), Some("os/repodata/repomd.xml"));
        assert!(!classify("Packages/c/curl-8.4.0-1.oe2403.x86_64.rpm").metadata);
        assert!(classify("RPM-GPG-KEY-openEuler").metadata);
    }
}
