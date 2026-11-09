//! Metadata persistence for Trove.
//!
//! Uses `sqlx`'s `Any` driver so the same queries run against SQLite (single
//! node, zero dependencies) and PostgreSQL / openGauss (HA deployments).
//! All SQL in this crate must therefore stay within the portable subset:
//! `$N` placeholders, `TEXT`/`BIGINT` columns, `ON CONFLICT ... DO UPDATE`.

use std::str::FromStr;

use serde_json::Value;
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, AssertSqlSafe, Row};
use trove_core::model::*;
use trove_core::util::{new_id, now_millis};
use trove_core::{Error, Result};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
    backend: Backend,
}

/// Aggregate figures for the dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoStats {
    pub repo_id: String,
    pub asset_count: i64,
    pub total_size: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AssetQuery {
    pub q: String,
    pub repo_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

/// Fields needed to write an asset.
#[derive(Debug, Clone)]
pub struct NewAsset<'a> {
    pub repo_id: &'a str,
    pub path: &'a str,
    pub blob: &'a BlobInfo,
    pub content_type: &'a str,
    pub uploader: &'a str,
    pub attributes: Value,
}

fn b(v: bool) -> i64 {
    v as i64
}

fn parse<T: FromStr<Err = Error>>(s: &str) -> Result<T> {
    s.parse()
}

impl Db {
    /// Connect and run pending migrations.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let backend = if url.starts_with("sqlite:") {
            Backend::Sqlite
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Backend::Postgres
        } else {
            return Err(Error::bad_request(format!("unsupported database url: {url}")));
        };
        if backend == Backend::Sqlite {
            ensure_sqlite_parent(url)?;
        }
        let max = if backend == Backend::Sqlite { max_connections.min(8) } else { max_connections };
        let pool = AnyPoolOptions::new()
            .max_connections(max.max(1))
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    if backend == Backend::Sqlite {
                        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
                        sqlx::query("PRAGMA journal_mode = WAL").execute(&mut *conn).await?;
                        sqlx::query("PRAGMA busy_timeout = 10000").execute(&mut *conn).await?;
                        sqlx::query("PRAGMA synchronous = NORMAL").execute(&mut *conn).await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .map_err(|e| Error::Internal(anyhow::anyhow!("connecting to database: {e}")))?;
        MIGRATOR.run(&pool).await.map_err(|e| Error::Internal(anyhow::anyhow!("running migrations: {e}")))?;
        Ok(Self { pool, backend })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- repositories

    const REPO_COLS: &'static str = "id, name, format, kind, online, description, config, created_at, updated_at";

    fn map_repo(row: &AnyRow) -> Result<Repository> {
        let config: String = row.try_get("config")?;
        Ok(Repository {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            format: parse(&row.try_get::<String, _>("format")?)?,
            kind: parse(&row.try_get::<String, _>("kind")?)?,
            online: row.try_get::<i64, _>("online")? != 0,
            description: row.try_get("description")?,
            config: serde_json::from_str(&config).map_err(Error::internal)?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    pub async fn list_repos(&self) -> Result<Vec<Repository>> {
        let sql = format!("SELECT {} FROM repositories ORDER BY name", Self::REPO_COLS);
        let rows = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_all(&self.pool).await?;
        rows.iter().map(Self::map_repo).collect()
    }

    pub async fn get_repo_by_name(&self, name: &str) -> Result<Option<Repository>> {
        let sql = format!("SELECT {} FROM repositories WHERE name = $1", Self::REPO_COLS);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(name).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_repo).transpose()
    }

    pub async fn get_repo(&self, id: &str) -> Result<Option<Repository>> {
        let sql = format!("SELECT {} FROM repositories WHERE id = $1", Self::REPO_COLS);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_repo).transpose()
    }

    pub async fn create_repo(&self, repo: &Repository) -> Result<()> {
        sqlx::query(
            "INSERT INTO repositories (id, name, format, kind, online, description, config, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&repo.id)
        .bind(&repo.name)
        .bind(repo.format.as_str())
        .bind(repo.kind.as_str())
        .bind(b(repo.online))
        .bind(&repo.description)
        .bind(serde_json::to_string(&repo.config)?)
        .bind(repo.created_at)
        .bind(repo.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::conflict(format!("repository '{}' already exists", repo.name)),
            other => other,
        })?;
        Ok(())
    }

    pub async fn update_repo(&self, repo: &Repository) -> Result<()> {
        let res = sqlx::query(
            "UPDATE repositories SET online = $2, description = $3, config = $4, updated_at = $5 WHERE id = $1",
        )
        .bind(&repo.id)
        .bind(b(repo.online))
        .bind(&repo.description)
        .bind(serde_json::to_string(&repo.config)?)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::not_found(format!("repository '{}'", repo.name)));
        }
        Ok(())
    }

    pub async fn delete_repo(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM assets WHERE repo_id = $1").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM repositories WHERE id = $1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    // ---------------------------------------------------------------- blobs

    pub async fn upsert_blob(&self, blob: &BlobInfo) -> Result<()> {
        sqlx::query(
            "INSERT INTO blobs (sha256, sha1, md5, sha512, size, created_at) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (sha256) DO NOTHING",
        )
        .bind(&blob.sha256)
        .bind(&blob.sha1)
        .bind(&blob.md5)
        .bind(&blob.sha512)
        .bind(blob.size)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Blobs that are no longer referenced by any asset and older than `older_than`.
    pub async fn orphan_blobs(&self, older_than: i64, limit: i64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT b.sha256 FROM blobs b
             WHERE b.created_at < $1 AND NOT EXISTS (SELECT 1 FROM assets a WHERE a.blob_sha256 = b.sha256)
             LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| r.try_get::<String, _>(0).map_err(Error::from)).collect()
    }

    pub async fn get_blob(&self, sha256: &str) -> Result<Option<BlobInfo>> {
        let row = sqlx::query("SELECT sha256, sha1, md5, sha512, size FROM blobs WHERE sha256 = $1")
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(BlobInfo {
                sha256: r.try_get("sha256")?,
                sha1: r.try_get("sha1")?,
                md5: r.try_get("md5")?,
                sha512: r.try_get("sha512")?,
                size: r.try_get("size")?,
            })
        })
        .transpose()
    }

    pub async fn blob_size(&self, sha256: &str) -> Result<Option<i64>> {
        let row =
            sqlx::query("SELECT size FROM blobs WHERE sha256 = $1").bind(sha256).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?)
    }

    /// Delete a blob row if it is still unreferenced. Returns true if deleted.
    pub async fn delete_blob_if_orphan(&self, sha256: &str) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM blobs WHERE sha256 = $1
             AND NOT EXISTS (SELECT 1 FROM assets a WHERE a.blob_sha256 = $1)",
        )
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    // ---------------------------------------------------------------- assets

    const ASSET_SELECT: &'static str = "SELECT a.id, a.repo_id, a.path, a.content_type, a.uploader, a.attributes,
        a.created_at, a.updated_at, a.last_downloaded_at,
        b.sha256, b.sha1, b.md5, b.sha512, b.size
        FROM assets a JOIN blobs b ON b.sha256 = a.blob_sha256";

    fn map_asset(row: &AnyRow) -> Result<Asset> {
        let attrs: String = row.try_get("attributes")?;
        Ok(Asset {
            id: row.try_get("id")?,
            repo_id: row.try_get("repo_id")?,
            path: row.try_get("path")?,
            content_type: row.try_get("content_type")?,
            uploader: row.try_get("uploader")?,
            attributes: serde_json::from_str(&attrs).unwrap_or(Value::Null),
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            last_downloaded_at: row.try_get("last_downloaded_at")?,
            blob: BlobInfo {
                sha256: row.try_get("sha256")?,
                sha1: row.try_get("sha1")?,
                md5: row.try_get("md5")?,
                sha512: row.try_get("sha512")?,
                size: row.try_get("size")?,
            },
        })
    }

    pub async fn get_asset(&self, repo_id: &str, path: &str) -> Result<Option<Asset>> {
        let sql = format!("{} WHERE a.repo_id = $1 AND a.path = $2", Self::ASSET_SELECT);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(repo_id).bind(path).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_asset).transpose()
    }

    pub async fn get_asset_by_id(&self, id: &str) -> Result<Option<Asset>> {
        let sql = format!("{} WHERE a.id = $1", Self::ASSET_SELECT);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_asset).transpose()
    }

    /// Insert or replace an asset. Returns the previous blob hash when replaced.
    pub async fn put_asset(&self, a: NewAsset<'_>) -> Result<(Asset, Option<String>)> {
        self.upsert_blob(a.blob).await?;
        let now = now_millis();
        let previous = sqlx::query("SELECT blob_sha256 FROM assets WHERE repo_id = $1 AND path = $2")
            .bind(a.repo_id)
            .bind(a.path)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.try_get::<String, _>(0))
            .transpose()?;
        sqlx::query(
            "INSERT INTO assets (id, repo_id, path, blob_sha256, content_type, uploader, attributes, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
             ON CONFLICT (repo_id, path) DO UPDATE SET
                blob_sha256 = excluded.blob_sha256,
                content_type = excluded.content_type,
                uploader = excluded.uploader,
                attributes = excluded.attributes,
                updated_at = CASE WHEN assets.blob_sha256 = excluded.blob_sha256
                                  THEN assets.updated_at ELSE excluded.updated_at END",
        )
        .bind(new_id())
        .bind(a.repo_id)
        .bind(a.path)
        .bind(&a.blob.sha256)
        .bind(a.content_type)
        .bind(a.uploader)
        .bind(a.attributes.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?;
        let asset =
            self.get_asset(a.repo_id, a.path).await?.ok_or_else(|| Error::internal("asset vanished after write"))?;
        Ok((asset, previous.filter(|p| p != &a.blob.sha256)))
    }

    pub async fn set_asset_attributes(&self, id: &str, attributes: &Value) -> Result<()> {
        sqlx::query("UPDATE assets SET attributes = $2 WHERE id = $1")
            .bind(id)
            .bind(attributes.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn touch_asset(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE assets SET last_downloaded_at = $2 WHERE id = $1")
            .bind(id)
            .bind(now_millis())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_asset(&self, repo_id: &str, path: &str) -> Result<Option<Asset>> {
        let Some(asset) = self.get_asset(repo_id, path).await? else { return Ok(None) };
        sqlx::query("DELETE FROM assets WHERE id = $1").bind(&asset.id).execute(&self.pool).await?;
        Ok(Some(asset))
    }

    /// Delete every asset at or below `prefix/`. Returns the deleted assets.
    pub async fn delete_assets_under(&self, repo_id: &str, prefix: &str) -> Result<Vec<Asset>> {
        let assets = self.list_assets_under(repo_id, prefix).await?;
        let mut tx = self.pool.begin().await?;
        for a in &assets {
            sqlx::query("DELETE FROM assets WHERE id = $1").bind(&a.id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(assets)
    }

    /// All assets whose path is `prefix` or starts with `prefix/` (all when empty).
    pub async fn list_assets_under(&self, repo_id: &str, prefix: &str) -> Result<Vec<Asset>> {
        let rows = if prefix.is_empty() {
            let sql = format!("{} WHERE a.repo_id = $1 ORDER BY a.path", Self::ASSET_SELECT);
            sqlx::query(AssertSqlSafe(sql.as_str())).bind(repo_id).fetch_all(&self.pool).await?
        } else {
            let with_slash = format!("{prefix}/");
            let sql = format!(
                "{} WHERE a.repo_id = $1 AND (a.path = $2 OR substr(a.path, 1, CAST($3 AS INTEGER)) = $4) ORDER BY a.path",
                Self::ASSET_SELECT
            );
            sqlx::query(AssertSqlSafe(sql.as_str()))
                .bind(repo_id)
                .bind(prefix)
                .bind(with_slash.chars().count() as i64)
                .bind(&with_slash)
                .fetch_all(&self.pool)
                .await?
        };
        rows.iter().map(Self::map_asset).collect()
    }

    pub async fn search_assets(&self, q: &AssetQuery) -> Result<(Vec<Asset>, i64)> {
        let pattern = format!("%{}%", q.q.to_lowercase().replace(['%', '_'], ""));
        let limit = if q.limit <= 0 { 50 } else { q.limit.min(500) };
        let (where_sql, has_repo) = match &q.repo_id {
            Some(_) => ("WHERE LOWER(a.path) LIKE $1 AND a.repo_id = $2", true),
            None => ("WHERE LOWER(a.path) LIKE $1", false),
        };
        let count_sql = format!("SELECT COUNT(*) FROM assets a {where_sql}");
        let mut count_q = sqlx::query(AssertSqlSafe(count_sql.as_str())).bind(&pattern);
        if has_repo {
            count_q = count_q.bind(q.repo_id.as_deref());
        }
        let total: i64 = count_q.fetch_one(&self.pool).await?.try_get(0)?;

        let (lim_idx, off_idx) = if has_repo { (3, 4) } else { (2, 3) };
        let sql =
            format!("{} {where_sql} ORDER BY a.updated_at DESC LIMIT ${lim_idx} OFFSET ${off_idx}", Self::ASSET_SELECT);
        let mut query = sqlx::query(AssertSqlSafe(sql.as_str())).bind(&pattern);
        if has_repo {
            query = query.bind(q.repo_id.as_deref());
        }
        let rows = query.bind(limit).bind(q.offset.max(0)).fetch_all(&self.pool).await?;
        let assets = rows.iter().map(Self::map_asset).collect::<Result<Vec<_>>>()?;
        Ok((assets, total))
    }

    pub async fn recent_assets(&self, limit: i64) -> Result<Vec<Asset>> {
        let sql = format!("{} ORDER BY a.updated_at DESC LIMIT $1", Self::ASSET_SELECT);
        let rows = sqlx::query(AssertSqlSafe(sql.as_str())).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(Self::map_asset).collect()
    }

    pub async fn repo_stats(&self) -> Result<Vec<RepoStats>> {
        let rows = sqlx::query(
            "SELECT a.repo_id, COUNT(*) AS cnt, CAST(COALESCE(SUM(b.size), 0) AS BIGINT) AS total
             FROM assets a JOIN blobs b ON b.sha256 = a.blob_sha256 GROUP BY a.repo_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(RepoStats {
                    repo_id: r.try_get("repo_id")?,
                    asset_count: r.try_get("cnt")?,
                    total_size: r.try_get("total")?,
                })
            })
            .collect()
    }

    /// Total number and size of distinct stored blobs.
    pub async fn blob_totals(&self) -> Result<(i64, i64)> {
        let row = sqlx::query("SELECT COUNT(*) AS cnt, CAST(COALESCE(SUM(size), 0) AS BIGINT) AS total FROM blobs")
            .fetch_one(&self.pool)
            .await?;
        Ok((row.try_get("cnt")?, row.try_get("total")?))
    }

    // ---------------------------------------------------------------- users

    const USER_COLS: &'static str = "id, username, display_name, email, role, password_hash, disabled, created_at";

    fn map_user(row: &AnyRow) -> Result<User> {
        Ok(User {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            display_name: row.try_get("display_name")?,
            email: row.try_get("email")?,
            role: parse(&row.try_get::<String, _>("role")?)?,
            password_hash: row.try_get("password_hash")?,
            disabled: row.try_get::<i64, _>("disabled")? != 0,
            created_at: row.try_get("created_at")?,
        })
    }

    pub async fn count_users(&self) -> Result<i64> {
        Ok(sqlx::query("SELECT COUNT(*) FROM users").fetch_one(&self.pool).await?.try_get(0)?)
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        let sql = format!("SELECT {} FROM users ORDER BY username", Self::USER_COLS);
        let rows = sqlx::query(AssertSqlSafe(sql.as_str())).fetch_all(&self.pool).await?;
        rows.iter().map(Self::map_user).collect()
    }

    pub async fn get_user_by_name(&self, username: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {} FROM users WHERE username = $1", Self::USER_COLS);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(username).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_user).transpose()
    }

    pub async fn get_user(&self, id: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {} FROM users WHERE id = $1", Self::USER_COLS);
        let row = sqlx::query(AssertSqlSafe(sql.as_str())).bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(Self::map_user).transpose()
    }

    pub async fn create_user(&self, u: &User) -> Result<()> {
        sqlx::query(
            "INSERT INTO users (id, username, display_name, email, role, password_hash, disabled, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
        )
        .bind(&u.id)
        .bind(&u.username)
        .bind(&u.display_name)
        .bind(&u.email)
        .bind(u.role.as_str())
        .bind(&u.password_hash)
        .bind(b(u.disabled))
        .bind(u.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::conflict(format!("user '{}' already exists", u.username)),
            other => other,
        })?;
        Ok(())
    }

    pub async fn update_user(&self, u: &User) -> Result<()> {
        sqlx::query(
            "UPDATE users SET display_name = $2, email = $3, role = $4, password_hash = $5, disabled = $6, updated_at = $7
             WHERE id = $1",
        )
        .bind(&u.id)
        .bind(&u.display_name)
        .bind(&u.email)
        .bind(u.role.as_str())
        .bind(&u.password_hash)
        .bind(b(u.disabled))
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_user(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM tokens WHERE user_id = $1").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM users WHERE id = $1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn count_admins(&self) -> Result<i64> {
        Ok(sqlx::query("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0")
            .fetch_one(&self.pool)
            .await?
            .try_get(0)?)
    }

    // ---------------------------------------------------------------- tokens

    fn map_token(row: &AnyRow) -> Result<Token> {
        Ok(Token {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            name: row.try_get("name")?,
            kind: parse(&row.try_get::<String, _>("kind")?)?,
            prefix: row.try_get("prefix")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
            last_used_at: row.try_get("last_used_at")?,
        })
    }

    pub async fn create_token(&self, t: &Token, token_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO tokens (id, user_id, name, kind, token_hash, prefix, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&t.id)
        .bind(&t.user_id)
        .bind(&t.name)
        .bind(t.kind.as_str())
        .bind(token_hash)
        .bind(&t.prefix)
        .bind(t.created_at)
        .bind(t.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve a token hash to its (non-expired) token and owning user.
    pub async fn find_token(&self, token_hash: &str) -> Result<Option<(Token, User)>> {
        let row = sqlx::query(
            "SELECT id, user_id, name, kind, prefix, created_at, expires_at, last_used_at
             FROM tokens WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let token = Self::map_token(&row)?;
        if token.expires_at.is_some_and(|e| e < now_millis()) {
            return Ok(None);
        }
        let Some(user) = self.get_user(&token.user_id).await? else { return Ok(None) };
        // Only record usage once a minute to avoid write amplification.
        if token.last_used_at.is_none_or(|t| now_millis() - t > 60_000) {
            sqlx::query("UPDATE tokens SET last_used_at = $2 WHERE id = $1")
                .bind(&token.id)
                .bind(now_millis())
                .execute(&self.pool)
                .await?;
        }
        Ok(Some((token, user)))
    }

    pub async fn list_tokens(&self, user_id: &str, kind: TokenKind) -> Result<Vec<Token>> {
        let rows = sqlx::query(
            "SELECT id, user_id, name, kind, prefix, created_at, expires_at, last_used_at
             FROM tokens WHERE user_id = $1 AND kind = $2 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::map_token).collect()
    }

    pub async fn delete_token(&self, user_id: &str, id: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM tokens WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete_token_by_hash(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM tokens WHERE token_hash = $1").bind(token_hash).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn purge_expired_tokens(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM tokens WHERE expires_at IS NOT NULL AND expires_at < $1")
            .bind(now_millis())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // ---------------------------------------------------------------- settings

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = $1").bind(key).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---------------------------------------------------------------- audit

    pub async fn insert_audit(&self, e: &AuditEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_events (id, ts, actor, action, target, detail, client_ip)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&e.id)
        .bind(e.ts)
        .bind(&e.actor)
        .bind(&e.action)
        .bind(&e.target)
        .bind(&e.detail)
        .bind(&e.client_ip)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_audit(&self, q: &AuditQuery) -> Result<(Vec<AuditEvent>, i64)> {
        let mut clauses = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(a) = q.actor.as_deref().filter(|s| !s.is_empty()) {
            args.push(a.to_string());
            clauses.push(format!("actor = ${}", args.len()));
        }
        if let Some(a) = q.action.as_deref().filter(|s| !s.is_empty()) {
            args.push(format!("{a}%"));
            clauses.push(format!("action LIKE ${}", args.len()));
        }
        let where_sql = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };

        let count_sql = format!("SELECT COUNT(*) FROM audit_events {where_sql}");
        let mut cq = sqlx::query(AssertSqlSafe(count_sql.as_str()));
        for a in &args {
            cq = cq.bind(a);
        }
        let total: i64 = cq.fetch_one(&self.pool).await?.try_get(0)?;

        let limit = if q.limit <= 0 { 100 } else { q.limit.min(1000) };
        let sql = format!(
            "SELECT id, ts, actor, action, target, detail, client_ip FROM audit_events {where_sql}
             ORDER BY ts DESC LIMIT ${} OFFSET ${}",
            args.len() + 1,
            args.len() + 2
        );
        let mut query = sqlx::query(AssertSqlSafe(sql.as_str()));
        for a in &args {
            query = query.bind(a);
        }
        let rows = query.bind(limit).bind(q.offset.max(0)).fetch_all(&self.pool).await?;
        let events = rows
            .iter()
            .map(|r| {
                Ok(AuditEvent {
                    id: r.try_get("id")?,
                    ts: r.try_get("ts")?,
                    actor: r.try_get("actor")?,
                    action: r.try_get("action")?,
                    target: r.try_get("target")?,
                    detail: r.try_get("detail")?,
                    client_ip: r.try_get("client_ip")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((events, total))
    }
}

fn ensure_sqlite_parent(url: &str) -> Result<()> {
    let path = url.trim_start_matches("sqlite://").trim_start_matches("sqlite:");
    let path = path.split('?').next().unwrap_or_default();
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
