//! YUM / DNF group repositories: one `baseurl` for several YUM repositories
//! (e.g. openEuler `OS` + `update` + `EPOL`), like Nexus' yum group.
//!
//! `repodata/repomd.xml` of a group is generated: the members' `primary`,
//! `filelists`, `other`, `updateinfo` and `modules` metadata are merged into
//! new files stored in the group, packages present in several members are
//! listed once (first member wins, by package checksum), and other metadata
//! (comps) is taken from the first member that has it. SQLite and zchunk
//! variants are dropped; dnf and yum fall back to the XML files.
//!
//! The merge is redone when any member's `repomd.xml` changes. Package files
//! and anything else are looked up in the members in order. The merged
//! `repomd.xml` cannot carry the upstream signature, so clients keep
//! `gpgcheck=1` (package signatures) with `repo_gpgcheck=0`.

use std::collections::HashSet;
use std::io::{self, Write};

use axum::http::Method;
use axum::response::Response;
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use trove_core::model::{Asset, RepoKind, Repository};
use trove_core::util::{normalize_path, now_millis};
use trove_core::{Error, Result};

use super::Ctx;
use super::path_proxy;
use crate::content;
use crate::state::AppState;

const REPOMD: &str = "repodata/repomd.xml";
/// Metadata types merged across members.
const MERGED: [&str; 5] = ["primary", "filelists", "other", "updateinfo", "modules"];

pub async fn handle(ctx: Ctx) -> Result<Response> {
    let path = normalize_path(&ctx.path)?;
    match ctx.method {
        Method::GET | Method::HEAD => {}
        // Admins may drop the generated metadata; it is rebuilt on the next request.
        Method::DELETE => return super::raw::delete_path(&ctx, &path).await,
        ref other => return Err(Error::MethodNotAllowed(format!("{other}: yum groups are read-only"))),
    }
    ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
    if path.is_empty() || ctx.path.ends_with('/') {
        return Err(Error::not_found("directory listings are not available for yum groups"));
    }
    if path.ends_with("repodata/repomd.xml.asc") || path.ends_with("repodata/repomd.xml.key") {
        return Err(Error::not_found(
            "a yum group's merged repomd.xml is not signed upstream; use repo_gpgcheck=0 with gpgcheck=1",
        ));
    }
    let asset = lookup(&ctx.state, &ctx.repo, &path, 0).await?.ok_or_else(|| Error::not_found(path.clone()))?;
    content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
}

/// Resolve a path in a YUM repository of any kind.
pub fn lookup<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    path: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        match repo.kind {
            RepoKind::Proxy => path_proxy::fetch_from(state, repo, path, super::yum::classify).await,
            RepoKind::Hosted => state.db.get_asset(&repo.id, path).await,
            RepoKind::Group => {
                if let Some(dir) = path.strip_suffix(REPOMD).filter(|d| d.is_empty() || d.ends_with('/')) {
                    return merged_repomd(state, repo, dir, depth).await;
                }
                // Generated metadata lives in the group itself.
                if let Some(a) = state.db.get_asset(&repo.id, path).await? {
                    return Ok(Some(a));
                }
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match lookup(state, &m, path, depth + 1).await {
                        Ok(Some(a)) => return Ok(Some(a)),
                        Ok(None) => {}
                        Err(e) => failure = Some(content::member_failed(&m, e)),
                    }
                }
                failure.map_or(Ok(None), Err)
            }
        }
    })
}

/// One `<data>` entry of a member's repomd.xml.
#[derive(Debug, Clone)]
struct DataEntry {
    kind: String,
    href: String,
    sha256: Option<String>,
    /// The entry's XML, copied verbatim for metadata that is not merged.
    block: String,
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn parse_repomd(text: &str) -> Vec<DataEntry> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<data ") {
        let Some(len) = rest[start..].find("</data>") else { break };
        let block = &rest[start..start + len + "</data>".len()];
        rest = &rest[start + block.len()..];
        let open_end = block.find('>').unwrap_or(block.len());
        let Some(kind) = attr(&block[..open_end], "type") else { continue };
        let Some(href) = block.find("<location").and_then(|i| attr(&block[i..], "href")) else { continue };
        let sha256 = block.find("<checksum").and_then(|i| {
            let tag_end = block[i..].find('>')? + i;
            if attr(&block[i..tag_end], "type").as_deref() != Some("sha256") {
                return None;
            }
            let value_end = block[tag_end..].find('<')? + tag_end;
            Some(block[tag_end + 1..value_end].trim().to_ascii_lowercase())
        });
        out.push(DataEntry { kind, href, sha256, block: block.to_string() });
    }
    out
}

/// Metadata kept from the first member that has it (comps, product ids…).
fn copied(kind: &str) -> bool {
    !MERGED.contains(&kind)
        && !kind.ends_with("_db")
        && !kind.ends_with("_zck")
        && !kind.starts_with("primary")
        && !kind.starts_with("filelists")
        && !kind.starts_with("other")
        && !kind.starts_with("updateinfo")
        && !kind.starts_with("modules")
}

/// The group's repomd.xml for `dir` (`""` or `"sub/dir/"`), rebuilt when a
/// member's repomd.xml changed.
async fn merged_repomd(state: &AppState, group: &Repository, dir: &str, depth: usize) -> Result<Option<Asset>> {
    let repomd_path = format!("{dir}{REPOMD}");
    let mut sources = Vec::new();
    let mut failed = false;
    for m in content::group_members(state, group, depth).await? {
        match lookup(state, &m, &repomd_path, depth + 1).await {
            Ok(Some(a)) => sources.push((m, a)),
            Ok(None) => {}
            Err(e) => {
                content::member_failed(&m, e);
                failed = true;
            }
        }
    }
    let source_id = {
        let mut h = Sha256::new();
        for (m, a) in &sources {
            h.update(format!("{}:{}\n", m.id, a.blob.sha256));
        }
        hex::encode(h.finalize())
    };
    let is_current = |a: &Asset| a.attributes.get("source").and_then(Value::as_str) == Some(source_id.as_str());
    let cached = state.db.get_asset(&group.id, &repomd_path).await?;
    if let Some(c) = cached.as_ref().filter(|c| is_current(c) || (failed && !sources.is_empty()) || sources.is_empty())
    {
        // Up to date — or a member is failing / all are gone: keep the last
        // complete merge rather than publishing a partial one.
        return Ok(Some(c.clone()));
    }
    if sources.is_empty() {
        return if failed {
            Err(Error::Upstream(format!("no member of '{}' is reachable", group.name)))
        } else {
            Ok(None)
        };
    }
    let lock = state.flight_lock(&format!("yum-group:{}:{dir}", group.id)).await;
    let _held = lock.lock().await;
    if let Some(c) = state.db.get_asset(&group.id, &repomd_path).await?.filter(is_current) {
        return Ok(Some(c));
    }
    match build(state, group, dir, &sources, depth, &source_id, cached.as_ref()).await {
        Ok(asset) => Ok(Some(asset)),
        Err(e) => {
            tracing::warn!(group = %group.name, dir, error = %e, "merging yum metadata failed");
            match cached {
                Some(c) => Ok(Some(c)),
                None => Err(e),
            }
        }
    }
}

/// Member metadata files to merge, per member: (kind, href, compressed bytes).
type Inputs = Vec<Vec<(String, String, Bytes)>>;

async fn build(
    state: &AppState,
    group: &Repository,
    dir: &str,
    sources: &[(Repository, Asset)],
    depth: usize,
    source_id: &str,
    previous: Option<&Asset>,
) -> Result<Asset> {
    let started = std::time::Instant::now();
    let mut inputs: Inputs = Vec::new();
    let mut copied_blocks: Vec<String> = Vec::new();
    let mut copied_kinds: HashSet<String> = HashSet::new();
    for (member, repomd) in sources {
        let text = content::read_asset(state, repomd).await?;
        let entries = parse_repomd(&String::from_utf8_lossy(&text));
        let mut files = Vec::new();
        for e in entries {
            if MERGED.contains(&e.kind.as_str()) {
                let path = normalize_path(&format!("{dir}{}", e.href))?;
                let asset = lookup(state, member, &path, depth + 1).await?.ok_or_else(|| {
                    Error::Upstream(format!("{}: {path} is listed in repomd.xml but missing", member.name))
                })?;
                if e.sha256.as_ref().is_some_and(|s| *s != asset.blob.sha256) {
                    return Err(Error::Upstream(format!("{}: {path} does not match repomd.xml", member.name)));
                }
                files.push((e.kind, e.href, content::read_asset(state, &asset).await?));
            } else if copied(&e.kind) && copied_kinds.insert(e.kind.clone()) {
                copied_blocks.push(e.block);
            }
        }
        inputs.push(files);
    }
    let merged = tokio::task::spawn_blocking(move || merge(&inputs))
        .await
        .map_err(Error::internal)?
        .map_err(|e| Error::Upstream(format!("cannot merge yum metadata: {e}")))?;

    let now = now_millis();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n",
    );
    xml.push_str(&format!("  <revision>{}</revision>\n", now / 1000));
    let mut files = Vec::new();
    for f in merged {
        let sha = hex::encode(Sha256::digest(&f.compressed));
        let href = format!("repodata/{sha}-{}.gz", f.file_name);
        let path = format!("{dir}{href}");
        xml.push_str(&format!(
            "  <data type=\"{}\">\n    <checksum type=\"sha256\">{sha}</checksum>\n    <open-checksum type=\"sha256\">{}</open-checksum>\n    <location href=\"{href}\"/>\n    <timestamp>{}</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
            f.kind,
            f.open_sha256,
            now / 1000,
            f.compressed.len(),
            f.open_size
        ));
        content::store_bytes(state, group, &path, f.compressed.into(), "trove", "application/gzip", json!({})).await?;
        files.push(path);
    }
    for block in copied_blocks {
        xml.push_str("  ");
        xml.push_str(&block);
        xml.push('\n');
    }
    xml.push_str("</repomd>\n");

    // Keep the previous generation for clients in the middle of an update;
    // the one before it goes.
    let prev_files: Vec<String> = previous
        .and_then(|p| p.attributes.get("files"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let older: Vec<String> = previous
        .and_then(|p| p.attributes.get("previous"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let attributes = json!({ "source": source_id, "files": files, "previous": prev_files });
    let asset = content::store_bytes(
        state,
        group,
        &format!("{dir}{REPOMD}"),
        Bytes::from(xml),
        "trove",
        "application/xml",
        attributes,
    )
    .await?;
    for old in older.iter().filter(|p| !files.contains(p) && !prev_files.contains(p)) {
        state.db.delete_asset(&group.id, old).await?;
    }
    tracing::info!(group = %group.name, dir, members = sources.len(), elapsed_ms = started.elapsed().as_millis() as u64, "merged yum metadata");
    Ok(asset)
}

// ------------------------------------------------------------------ merge

struct Merged {
    kind: &'static str,
    /// Without the `.gz` suffix, e.g. `primary.xml`.
    file_name: &'static str,
    compressed: Vec<u8>,
    open_sha256: String,
    open_size: u64,
}

/// Uncompressed-side hashing and counting on top of a gzip encoder.
struct Output {
    gz: flate2::write::GzEncoder<Vec<u8>>,
    sha: Sha256,
    size: u64,
}

impl Output {
    fn new() -> Self {
        Self { gz: flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6)), sha: Sha256::new(), size: 0 }
    }
    fn put(&mut self, data: &[u8]) -> io::Result<()> {
        self.sha.update(data);
        self.size += data.len() as u64;
        self.gz.write_all(data)
    }
    fn finish(self, kind: &'static str, file_name: &'static str) -> io::Result<Merged> {
        Ok(Merged {
            kind,
            file_name,
            compressed: self.gz.finish()?,
            open_sha256: hex::encode(self.sha.finalize()),
            open_size: self.size,
        })
    }
}

/// Splits an XML stream into header, top-level elements and trailer without
/// holding more than one element in memory.
struct Splitter<'f> {
    open: &'static [u8],
    close: &'static [u8],
    buf: Vec<u8>,
    in_body: bool,
    scan: usize,
    header: Option<Vec<u8>>,
    on_element: &'f mut dyn FnMut(&[u8]) -> io::Result<()>,
}

impl<'f> Splitter<'f> {
    fn new(element: &'static str, on_element: &'f mut dyn FnMut(&[u8]) -> io::Result<()>) -> Self {
        let (open, close): (&'static [u8], &'static [u8]) = match element {
            "package" => (b"<package", b"</package>"),
            _ => (b"<update", b"</update>"),
        };
        Self { open, close, buf: Vec::new(), in_body: false, scan: 0, header: None, on_element }
    }

    fn process(&mut self) -> io::Result<()> {
        loop {
            if !self.in_body {
                let found = find_tag(&self.buf, self.open, self.scan);
                match found {
                    Some(i) => {
                        self.header = Some(self.buf[..i].to_vec());
                        self.buf.drain(..i);
                        self.in_body = true;
                        self.scan = 0;
                    }
                    None => {
                        self.scan = self.buf.len().saturating_sub(self.open.len() + 1);
                        return Ok(());
                    }
                }
            } else {
                match find(&self.buf, self.close, self.scan) {
                    Some(j) => {
                        let end = j + self.close.len();
                        let start = self.buf[..end].iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
                        (self.on_element)(&self.buf[start..end])?;
                        self.buf.drain(..end);
                        self.scan = 0;
                    }
                    None => {
                        self.scan = self.buf.len().saturating_sub(self.close.len());
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Header and trailer of the document.
    fn finish(mut self) -> (Option<Vec<u8>>, Vec<u8>) {
        if self.header.is_none() {
            // No elements: split before the root's closing tag.
            if let Some(i) = self.buf.windows(2).rposition(|w| w == b"</") {
                let trailer = self.buf.split_off(i);
                return (Some(std::mem::take(&mut self.buf)), trailer);
            }
            return (None, Vec::new());
        }
        let trailer = std::mem::take(&mut self.buf);
        (self.header.take(), trailer)
    }
}

impl Write for Splitter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.process()?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Find `<name` followed by whitespace or `>` (so `<package` ≠ `<packager`).
fn find_tag(hay: &[u8], open: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(i) = find(hay, open, at) {
        match hay.get(i + open.len()) {
            Some(b) if b.is_ascii_whitespace() || *b == b'>' => return Some(i),
            Some(_) => at = i + 1,
            None => return None, // need more data
        }
    }
    None
}

/// Decompress `data` (by file extension) into `sink`.
fn decompress(href: &str, data: &[u8], sink: &mut dyn Write) -> io::Result<()> {
    if href.ends_with(".gz") {
        io::copy(&mut flate2::read::MultiGzDecoder::new(data), sink)?;
    } else if href.ends_with(".xz") {
        lzma_rs::xz_decompress(&mut io::BufReader::new(data), &mut &mut *sink)
            .map_err(|e| io::Error::other(format!("{href}: {e}")))?;
    } else if href.ends_with(".zst") {
        let mut d =
            ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| io::Error::other(format!("{href}: {e}")))?;
        io::copy(&mut d, sink)?;
    } else if href.ends_with(".bz2") {
        io::copy(&mut bzip2::read::MultiBzDecoder::new(data), sink)?;
    } else if href.ends_with(".xml") || href.ends_with(".yaml") {
        sink.write_all(data)?;
    } else {
        return Err(io::Error::other(format!("{href}: unsupported compression")));
    }
    Ok(())
}

fn text_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(s[i..j].trim())
}

/// Package checksum: `<checksum … pkgid="YES">HEX</checksum>` in primary.
fn primary_pkgid(pkg: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(pkg).ok()?;
    let i = s.find("pkgid=\"YES\"")?;
    let rest = &s[i..];
    Some(rest[rest.find('>')? + 1..rest.find('<')?].trim().to_string())
}

/// `<package pkgid="HEX" …>` in filelists and other.
fn attr_pkgid(pkg: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(&pkg[..pkg.len().min(512)]).ok()?;
    attr(&s[..s.find('>').unwrap_or(s.len())], "pkgid")
}

fn update_id(update: &[u8]) -> Option<String> {
    text_between(std::str::from_utf8(update).ok()?, "<id>", "</id>").map(str::to_string)
}

/// Replace the `packages="N"` count of a root element.
fn set_count(header: &[u8], count: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(header);
    let Some(i) = text.find("packages=\"") else { return header.to_vec() };
    let start = i + "packages=\"".len();
    let Some(len) = text[start..].find('"') else { return header.to_vec() };
    format!("{}{count}{}", &text[..start], &text[start + len..]).into_bytes()
}

fn files_of<'a>(inputs: &'a Inputs, kind: &str) -> Vec<(usize, &'a str, &'a [u8])> {
    inputs
        .iter()
        .enumerate()
        .flat_map(|(i, files)| {
            files.iter().filter(|(k, _, _)| k == kind).map(move |(_, href, data)| (i, href.as_str(), &data[..]))
        })
        .collect()
}

fn merge(inputs: &Inputs) -> io::Result<Vec<Merged>> {
    let members = inputs.len();
    // Pass 1: which member provides each package (first one wins).
    let primaries = files_of(inputs, "primary");
    let mut accepted: Vec<HashSet<String>> = vec![HashSet::new(); members];
    let mut seen = HashSet::new();
    for &(i, href, data) in &primaries {
        let mut on = |pkg: &[u8]| {
            if let Some(id) = primary_pkgid(pkg)
                && seen.insert(id.clone())
            {
                accepted[i].insert(id);
            }
            Ok(())
        };
        let mut s = Splitter::new("package", &mut on);
        decompress(href, data, &mut s)?;
    }
    let mut out = Vec::new();
    for (kind, file_name, pkgid) in [
        ("primary", "primary.xml", primary_pkgid as fn(&[u8]) -> Option<String>),
        ("filelists", "filelists.xml", attr_pkgid),
        ("other", "other.xml", attr_pkgid),
    ] {
        let files = files_of(inputs, kind);
        if files.is_empty() {
            continue;
        }
        let count = files.iter().map(|(i, _, _)| *i).collect::<HashSet<_>>().iter().map(|i| accepted[*i].len()).sum();
        out.push(merge_elements(&files, kind, file_name, "package", Some(count), |i, el, emitted| {
            pkgid(el).is_some_and(|id| accepted[i].contains(&id) && emitted.insert(id))
        })?);
    }
    let updates = files_of(inputs, "updateinfo");
    if !updates.is_empty() {
        out.push(merge_elements(&updates, "updateinfo", "updateinfo.xml", "update", None, |_, el, emitted| {
            update_id(el).is_none_or(|id| emitted.insert(id))
        })?);
    }
    let modules = files_of(inputs, "modules");
    if !modules.is_empty() {
        // modules.yaml is a stream of YAML documents: concatenation merges.
        let mut o = Output::new();
        for (_, href, data) in modules {
            let mut buf = Vec::new();
            decompress(href, data, &mut buf)?;
            o.put(&buf)?;
            if !buf.ends_with(b"\n") {
                o.put(b"\n")?;
            }
        }
        out.push(o.finish("modules", "modules.yaml")?);
    }
    Ok(out)
}

/// Concatenate the elements of several XML documents of the same kind, keeping
/// the first document's header and trailer.
fn merge_elements(
    files: &[(usize, &str, &[u8])],
    kind: &'static str,
    file_name: &'static str,
    element: &'static str,
    count: Option<usize>,
    keep: impl Fn(usize, &[u8], &mut HashSet<String>) -> bool,
) -> io::Result<Merged> {
    // Elements are buffered compressed (gzip) until the header is known.
    let mut body = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut emitted = HashSet::new();
    let mut header = None;
    let mut trailer = Vec::new();
    for &(i, href, data) in files {
        let mut on = |el: &[u8]| {
            if keep(i, el, &mut emitted) {
                body.write_all(el)?;
                body.write_all(b"\n")?;
            }
            Ok(())
        };
        let mut s = Splitter::new(element, &mut on);
        decompress(href, data, &mut s)?;
        let (h, t) = s.finish();
        if header.is_none() && h.is_some() {
            header = h;
            trailer = t;
        }
    }
    let header = header.ok_or_else(|| io::Error::other(format!("{kind}: no document header")))?;
    let mut o = Output::new();
    let header = match count {
        Some(n) => set_count(&header, n),
        None => header,
    };
    o.put(&header)?;
    if !header.ends_with(b"\n") {
        o.put(b"\n")?;
    }
    let body = body.finish()?;
    let mut reader = flate2::read::GzDecoder::new(&body[..]);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = io::Read::read(&mut reader, &mut chunk)?;
        if n == 0 {
            break;
        }
        o.put(&chunk[..n])?;
    }
    o.put(trailer.trim_ascii_start())?;
    if !trailer.ends_with(b"\n") {
        o.put(b"\n")?;
    }
    o.finish(kind, file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz(s: &str) -> Bytes {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(s.as_bytes()).unwrap();
        e.finish().unwrap().into()
    }

    fn primary(pkgs: &[(&str, &str)]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"{}\">\n",
            pkgs.len()
        );
        for (name, id) in pkgs {
            s.push_str(&format!(
                "<package type=\"rpm\">\n  <name>{name}</name>\n  <checksum type=\"sha256\" pkgid=\"YES\">{id}</checksum>\n  <packager>x</packager>\n  <location href=\"Packages/{name}.rpm\"/>\n</package>\n"
            ));
        }
        s.push_str("</metadata>\n");
        s
    }

    fn filelists(pkgs: &[(&str, &str)]) -> String {
        let mut s = format!(
            "<?xml version=\"1.0\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"{}\">\n",
            pkgs.len()
        );
        for (name, id) in pkgs {
            s.push_str(&format!(
                "<package pkgid=\"{id}\" name=\"{name}\" arch=\"noarch\"><file>/usr/bin/{name}</file></package>\n"
            ));
        }
        s.push_str("</filelists>\n");
        s
    }

    fn unpack(m: &Merged) -> String {
        let mut out = String::new();
        io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&m.compressed[..]), &mut out).unwrap();
        assert_eq!(hex::encode(Sha256::digest(out.as_bytes())), m.open_sha256);
        out
    }

    #[test]
    fn merges_and_dedupes_packages() {
        let a = [("curl", "aa"), ("bash", "bb")];
        let b = [("bash", "bb"), ("vim", "cc")];
        let inputs: Inputs = vec![
            vec![
                ("primary".into(), "repodata/x-primary.xml.gz".into(), gz(&primary(&a))),
                ("filelists".into(), "repodata/x-filelists.xml.gz".into(), gz(&filelists(&a))),
            ],
            vec![
                ("primary".into(), "repodata/y-primary.xml".into(), primary(&b).into_bytes().into()),
                ("filelists".into(), "repodata/y-filelists.xml.gz".into(), gz(&filelists(&b))),
                (
                    "updateinfo".into(),
                    "repodata/u.xml".into(),
                    Bytes::from(
                        "<?xml version=\"1.0\"?>\n<updates>\n<update type=\"security\"><id>SA-1</id></update>\n</updates>\n",
                    ),
                ),
            ],
        ];
        let merged = merge(&inputs).unwrap();
        let kinds: Vec<_> = merged.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, ["primary", "filelists", "updateinfo"]);
        let p = unpack(&merged[0]);
        assert!(p.contains("packages=\"3\""), "{p}");
        assert_eq!(p.matches("<package type").count(), 3);
        assert_eq!(p.matches("<name>bash</name>").count(), 1);
        assert!(p.trim_end().ends_with("</metadata>"));
        assert!(p.contains("<packager>x</packager>"));
        let f = unpack(&merged[1]);
        assert!(f.contains("packages=\"3\""));
        assert_eq!(f.matches("<package pkgid").count(), 3);
        assert!(f.trim_end().ends_with("</filelists>"));
        let u = unpack(&merged[2]);
        assert!(u.contains("<id>SA-1</id>") && u.trim_end().ends_with("</updates>"), "{u}");
    }

    #[test]
    fn splitter_handles_tiny_writes_and_empty_documents() {
        let doc = primary(&[("a", "1"), ("b", "2")]);
        let mut n = 0;
        let mut on = |_: &[u8]| {
            n += 1;
            Ok(())
        };
        let mut s = Splitter::new("package", &mut on);
        for b in doc.as_bytes() {
            s.write_all(&[*b]).unwrap();
        }
        let (h, t) = s.finish();
        assert!(String::from_utf8(h.unwrap()).unwrap().ends_with("packages=\"2\">\n"));
        assert_eq!(String::from_utf8(t).unwrap().trim(), "</metadata>");
        assert_eq!(n, 2);

        let empty = primary(&[]);
        let mut on = |_: &[u8]| Ok(());
        let mut s = Splitter::new("package", &mut on);
        s.write_all(empty.as_bytes()).unwrap();
        let (h, t) = s.finish();
        assert!(h.is_some());
        assert_eq!(String::from_utf8(t).unwrap().trim(), "</metadata>");
    }

    #[test]
    fn decompresses_every_repodata_format() {
        let doc = primary(&[("a", "1")]);
        let zst = ruzstd::encoding::compress_to_vec(doc.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut doc.as_bytes(), &mut xz).unwrap();
        let mut bz = Vec::new();
        io::copy(&mut bzip2::read::BzEncoder::new(doc.as_bytes(), bzip2::Compression::fast()), &mut bz).unwrap();
        for (href, data) in [
            ("p.xml.zst", zst),
            ("p.xml.xz", xz),
            ("p.xml.bz2", bz),
            ("p.xml.gz", gz(&doc).to_vec()),
            ("p.xml", doc.clone().into_bytes()),
        ] {
            let mut out = Vec::new();
            decompress(href, &data, &mut out).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), doc, "{href}");
        }
        assert!(decompress("p.xml.zck", b"x", &mut Vec::new()).is_err());
    }

    #[test]
    fn repomd_parsing() {
        let xml = r#"<repomd><revision>1</revision>
<data type="primary"><checksum type="sha256">ABC</checksum><location href="repodata/abc-primary.xml.zst"/></data>
<data type="primary_db"><checksum type="sha256">d</checksum><location href="repodata/d-primary.sqlite.bz2"/></data>
<data type="group"><checksum type="sha1">e</checksum><location href="repodata/comps.xml"/></data>
</repomd>"#;
        let e = parse_repomd(xml);
        assert_eq!(e.len(), 3);
        assert_eq!(
            (e[0].kind.as_str(), e[0].href.as_str(), e[0].sha256.as_deref()),
            ("primary", "repodata/abc-primary.xml.zst", Some("abc"))
        );
        assert!(!copied("primary_db") && !copied("filelists_ext") && !copied("primary_zck"));
        assert!(copied("group") && copied("group_gz"));
        assert_eq!(e[2].sha256, None);
    }
}
