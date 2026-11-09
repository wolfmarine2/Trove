//! Small helpers shared across crates.

use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngExt;
use sha2::{Digest, Sha256};

use crate::error::Error;

/// Milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generate a random URL-safe secret with the given prefix, e.g. `trv_...`.
pub fn random_secret(prefix: &str, len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let body: String = (0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect();
    format!("{prefix}{body}")
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(data.as_ref()))
}

/// Repository / user names: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::bad_request(format!("invalid name '{name}': use letters, digits, '.', '_' or '-' (max 64)")))
    }
}

/// Normalise an artifact path: strip leading/trailing slashes, collapse empty
/// segments and reject traversal. Returns an error for unsafe input.
pub fn normalize_path(path: &str) -> Result<String, Error> {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(Error::bad_request("path traversal is not allowed")),
            s if s.contains('\\') || s.contains('\0') => {
                return Err(Error::bad_request("invalid character in path"));
            }
            s => out.push(s),
        }
    }
    Ok(out.join("/"))
}

/// PEP 503 project name normalisation.
pub fn pep503_normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_sep {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    out
}

/// Path of a crate inside a Cargo index (`1/a`, `2/ab`, `3/a/abc`, `se/rd/serde`).
pub fn cargo_index_path(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.len() {
        0 => String::new(),
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalisation() {
        assert_eq!(normalize_path("/a//b/./c/").unwrap(), "a/b/c");
        assert!(normalize_path("a/../b").is_err());
        assert_eq!(normalize_path("").unwrap(), "");
    }

    #[test]
    fn pep503() {
        assert_eq!(pep503_normalize("Friendly-Bard"), "friendly-bard");
        assert_eq!(pep503_normalize("FRIENDLY._.bard"), "friendly-bard");
        assert_eq!(pep503_normalize("friendly__bard"), "friendly-bard");
    }

    #[test]
    fn cargo_paths() {
        assert_eq!(cargo_index_path("a"), "1/a");
        assert_eq!(cargo_index_path("ab"), "2/ab");
        assert_eq!(cargo_index_path("abc"), "3/a/abc");
        assert_eq!(cargo_index_path("Serde"), "se/rd/serde");
    }

    #[test]
    fn names() {
        assert!(validate_name("npm-hosted").is_ok());
        assert!(validate_name("-bad").is_err());
        assert!(validate_name("a/b").is_err());
    }
}
