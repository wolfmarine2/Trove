//! Content-addressed blob storage.
//!
//! Every uploaded or cached file is streamed to a local temporary file while
//! its SHA-1 / SHA-256 / SHA-512 / MD5 digests are computed, then moved into
//! the backing store under `sha256/<aa>/<bb>/<sha256>`. Identical content is
//! stored once no matter how many repositories reference it.
//!
//! Backends: local filesystem (default) and any S3-compatible object store
//! (AWS S3, MinIO, Ceph RGW) via the `s3` feature.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt, stream::BoxStream};
use md5::Md5;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use trove_core::config::{Config, StorageKind};
use trove_core::model::BlobInfo;
use trove_core::{Error, Result};

/// A stream of blob bytes suitable for an HTTP response body.
pub type ByteStream = BoxStream<'static, std::io::Result<Bytes>>;

const MULTIPART_CHUNK: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct BlobStore {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn ObjectStore>,
    /// When set, blobs live on this local filesystem root and ingest can use
    /// an atomic rename instead of copying.
    fs_root: Option<PathBuf>,
    tmp_dir: PathBuf,
    description: String,
}

struct Hashers {
    sha1: Sha1,
    sha256: Sha256,
    sha512: Sha512,
    md5: Md5,
    size: u64,
}

impl Hashers {
    fn new() -> Self {
        Self { sha1: Sha1::new(), sha256: Sha256::new(), sha512: Sha512::new(), md5: Md5::new(), size: 0 }
    }
    fn update(&mut self, data: &[u8]) {
        self.sha1.update(data);
        self.sha256.update(data);
        self.sha512.update(data);
        self.md5.update(data);
        self.size += data.len() as u64;
    }
    fn finish(self) -> BlobInfo {
        BlobInfo {
            sha1: hex::encode(self.sha1.finalize()),
            sha256: hex::encode(self.sha256.finalize()),
            sha512: hex::encode(self.sha512.finalize()),
            md5: hex::encode(self.md5.finalize()),
            size: self.size as i64,
        }
    }
}

fn store_err(e: object_store::Error) -> Error {
    match e {
        object_store::Error::NotFound { path, .. } => Error::not_found(format!("blob {path}")),
        other => Error::Internal(anyhow::anyhow!("blob store: {other}")),
    }
}

impl BlobStore {
    /// Build the store described by the configuration.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let tmp_dir = cfg.server.data_dir.join("tmp");
        match cfg.storage.kind {
            StorageKind::Filesystem => Self::filesystem(&cfg.storage_path()),
            StorageKind::S3 => Self::s3(cfg, tmp_dir),
        }
    }

    pub fn filesystem(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let tmp_dir = root.join(".tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let store = LocalFileSystem::new_with_prefix(&root).map_err(store_err)?;
        Ok(Self {
            inner: Arc::new(Inner {
                store: Arc::new(store),
                description: format!("filesystem:{}", root.display()),
                fs_root: Some(root),
                tmp_dir,
            }),
        })
    }

    #[cfg(feature = "s3")]
    fn s3(cfg: &Config, tmp_dir: PathBuf) -> Result<Self> {
        use object_store::aws::AmazonS3Builder;
        let s3 = &cfg.storage.s3;
        if s3.bucket.is_empty() {
            return Err(Error::bad_request("storage.s3.bucket is required"));
        }
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&s3.bucket);
        if !s3.region.is_empty() {
            builder = builder.with_region(&s3.region);
        }
        if !s3.endpoint.is_empty() {
            builder = builder.with_endpoint(&s3.endpoint);
        }
        if !s3.access_key_id.is_empty() {
            builder = builder.with_access_key_id(&s3.access_key_id).with_secret_access_key(&s3.secret_access_key);
        }
        builder = builder.with_allow_http(s3.allow_http).with_virtual_hosted_style_request(!s3.path_style);
        let store = builder.build().map_err(store_err)?;
        let store: Arc<dyn ObjectStore> = if s3.prefix.is_empty() {
            Arc::new(store)
        } else {
            Arc::new(object_store::prefix::PrefixStore::new(store, s3.prefix.as_str()))
        };
        std::fs::create_dir_all(&tmp_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                fs_root: None,
                tmp_dir,
                description: format!("s3://{}/{}", s3.bucket, s3.prefix),
            }),
        })
    }

    #[cfg(not(feature = "s3"))]
    fn s3(_cfg: &Config, _tmp_dir: PathBuf) -> Result<Self> {
        Err(Error::bad_request("this build of Trove was compiled without S3 support"))
    }

    pub fn description(&self) -> &str {
        &self.inner.description
    }

    fn location(sha256: &str) -> Result<ObjPath> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::bad_request("invalid blob id"));
        }
        Ok(ObjPath::from(format!("sha256/{}/{}/{}", &sha256[..2], &sha256[2..4], sha256)))
    }

    /// Stream `body` into the store. Fails with `PayloadTooLarge` past `max_size` bytes.
    pub async fn ingest<S, E>(&self, body: S, max_size: u64) -> Result<BlobInfo>
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send,
        E: std::fmt::Display,
    {
        let tmp = tempfile::Builder::new().prefix("upload-").tempfile_in(&self.inner.tmp_dir)?.into_temp_path();
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hashers = Hashers::new();
        let mut body = std::pin::pin!(body);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| Error::bad_request(format!("reading upload: {e}")))?;
            hashers.update(&chunk);
            if hashers.size > max_size {
                return Err(Error::PayloadTooLarge);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let info = hashers.finish();
        self.commit(&tmp, &info).await?;
        Ok(info)
    }

    /// Store an in-memory buffer (generated metadata, small files).
    pub async fn ingest_bytes(&self, data: Bytes) -> Result<BlobInfo> {
        let len = data.len() as u64;
        self.ingest(futures::stream::iter([Ok::<_, std::io::Error>(data)]), len).await
    }

    async fn commit(&self, tmp: &Path, info: &BlobInfo) -> Result<()> {
        let location = Self::location(&info.sha256)?;
        if self.exists(&info.sha256).await? {
            return Ok(()); // deduplicated
        }
        if let Some(root) = &self.inner.fs_root {
            let dest = root.join(location.as_ref());
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(tmp, &dest).await?;
            return Ok(());
        }
        // Remote store: single PUT for small blobs, multipart otherwise.
        let mut file = tokio::fs::File::open(tmp).await?;
        if (info.size as usize) <= MULTIPART_CHUNK {
            let mut buf = Vec::with_capacity(info.size as usize);
            file.read_to_end(&mut buf).await?;
            self.inner.store.put(&location, PutPayload::from(buf)).await.map_err(store_err)?;
            return Ok(());
        }
        let upload = self.inner.store.put_multipart(&location).await.map_err(store_err)?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, MULTIPART_CHUNK);
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.wait_for_capacity(4).await.map_err(store_err)?;
            writer.write(&buf[..n]);
        }
        writer.finish().await.map_err(store_err)?;
        Ok(())
    }

    pub async fn exists(&self, sha256: &str) -> Result<bool> {
        match self.inner.store.head(&Self::location(sha256)?).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(store_err(e)),
        }
    }

    /// Open a blob for streaming.
    pub async fn open(&self, sha256: &str) -> Result<ByteStream> {
        let res = self.inner.store.get(&Self::location(sha256)?).await.map_err(store_err)?;
        Ok(res.into_stream().map_err(std::io::Error::other).boxed())
    }

    /// Read a whole blob into memory (for metadata documents). Bounded by `limit`.
    pub async fn read_all(&self, sha256: &str, limit: usize) -> Result<Bytes> {
        let mut stream = self.open(sha256).await?;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if out.len() + chunk.len() > limit {
                return Err(Error::PayloadTooLarge);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(out))
    }

    pub async fn delete(&self, sha256: &str) -> Result<()> {
        match self.inner.store.delete(&Self::location(sha256)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(store_err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ingest_dedupe_read_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::filesystem(dir.path()).unwrap();
        let info = store.ingest_bytes(Bytes::from_static(b"hello world")).await.unwrap();
        assert_eq!(info.size, 11);
        assert_eq!(info.sha256, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
        assert_eq!(info.sha1, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
        assert_eq!(info.md5, "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert!(store.exists(&info.sha256).await.unwrap());

        // second ingest of the same content is deduplicated
        let again = store.ingest_bytes(Bytes::from_static(b"hello world")).await.unwrap();
        assert_eq!(again.sha256, info.sha256);

        let data = store.read_all(&info.sha256, 1024).await.unwrap();
        assert_eq!(&data[..], b"hello world");
        assert!(matches!(store.read_all(&info.sha256, 4).await, Err(Error::PayloadTooLarge)));

        store.delete(&info.sha256).await.unwrap();
        assert!(!store.exists(&info.sha256).await.unwrap());
        assert!(matches!(store.open(&info.sha256).await, Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn size_limit_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::filesystem(dir.path()).unwrap();
        let chunks = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 600])),
            Ok(Bytes::from(vec![0u8; 600])),
        ]);
        assert!(matches!(store.ingest(chunks, 1000).await, Err(Error::PayloadTooLarge)));
        // temp file must be cleaned up
        assert_eq!(std::fs::read_dir(dir.path().join(".tmp")).unwrap().count(), 0);
    }

    #[test]
    fn rejects_bad_ids() {
        assert!(BlobStore::location("../etc/passwd").is_err());
    }
}
