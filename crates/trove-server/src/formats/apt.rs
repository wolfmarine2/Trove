//! APT (Debian / Ubuntu / Kylin / UOS) proxy.
//!
//! ```text
//! deb http://trove.example.com/repository/ubuntu/ noble main restricted universe
//! ```
//!
//! * `dists/**` (InRelease, Release, Packages, …) is metadata, refreshed after
//!   the repository's max age. Index files are tied to their suite's
//!   InRelease/Release: when the signed file changes they are refetched, which
//!   avoids "Hash Sum mismatch" errors.
//! * `pool/**` and `dists/**/by-hash/**` are immutable content.
//!
//! Signatures are passed through untouched, so clients keep verifying with the
//! distribution's own keys.

use axum::response::Response;
use trove_core::Result;

use super::Ctx;
use super::path_proxy::{self, Policy};

pub fn classify(path: &str) -> Policy {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.contains(&"by-hash") {
        return Policy { metadata: false, anchor: None };
    }
    let Some(idx) = segs.iter().position(|s| *s == "dists") else {
        return Policy { metadata: false, anchor: None };
    };
    // dists/<suite>/... ; the suite may itself contain slashes (e.g. "noble-security"
    // is one segment, but "stable/updates" was two in old Debian), so anchor on the
    // closest InRelease above the file.
    let file = segs.last().copied().unwrap_or_default();
    let is_signature = matches!(file, "InRelease" | "Release" | "Release.gpg");
    let anchor = if is_signature || segs.len() < idx + 3 {
        None
    } else {
        Some(format!("{}/InRelease", segs[..idx + 2].join("/")))
    };
    Policy { metadata: true, anchor }
}

pub async fn handle(ctx: Ctx) -> Result<Response> {
    path_proxy::handle(ctx, classify).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let p = classify("dists/noble/InRelease");
        assert!(p.metadata && p.anchor.is_none());
        let p = classify("dists/noble/main/binary-amd64/Packages.xz");
        assert!(p.metadata);
        assert_eq!(p.anchor.as_deref(), Some("dists/noble/InRelease"));
        assert!(!classify("dists/noble/main/binary-amd64/by-hash/SHA256/abcd").metadata);
        assert!(!classify("pool/main/c/curl/curl_8.5.0-2ubuntu10_amd64.deb").metadata);
        let p = classify("ubuntu/dists/jammy/main/i18n/Translation-en.bz2");
        assert_eq!(p.anchor.as_deref(), Some("ubuntu/dists/jammy/InRelease"));
    }
}
