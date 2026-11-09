use super::*;

/// Tests share one PostgreSQL database, so they must not run concurrently.
static PG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn databases() -> Vec<(Db, Option<tempfile::TempDir>)> {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("t.db").display());
    let mut dbs = vec![(Db::connect(&url, 4).await.unwrap(), Some(dir))];
    // Optionally exercise PostgreSQL: TROVE_TEST_POSTGRES_URL=postgres://...
    if let Ok(pg) = std::env::var("TROVE_TEST_POSTGRES_URL") {
        let db = Db::connect(&pg, 4).await.unwrap();
        for t in ["audit_events", "settings", "tokens", "users", "assets", "blobs", "repositories"] {
            sqlx::query(AssertSqlSafe(format!("DELETE FROM {t}"))).execute(&db.pool).await.unwrap();
        }
        dbs.push((db, None));
    }
    dbs
}

fn repo(name: &str) -> Repository {
    Repository {
        id: new_id(),
        name: name.into(),
        format: Format::Raw,
        kind: RepoKind::Hosted,
        online: true,
        description: "test".into(),
        config: RepoConfig::default(),
        created_at: now_millis(),
        updated_at: now_millis(),
    }
}

fn blob(seed: &str, size: i64) -> BlobInfo {
    BlobInfo {
        sha256: trove_core::util::sha256_hex(seed),
        sha1: "s1".into(),
        md5: "m5".into(),
        sha512: "s512".into(),
        size,
    }
}

#[tokio::test]
async fn repositories_and_assets() {
    let _serial = PG_LOCK.lock().await;
    for (db, _guard) in databases().await {
        let r = repo("raw-hosted");
        db.create_repo(&r).await.unwrap();
        assert!(matches!(db.create_repo(&r).await, Err(Error::Conflict(_))));
        let got = db.get_repo_by_name("raw-hosted").await.unwrap().unwrap();
        assert_eq!(got.format, Format::Raw);
        assert!(got.online);

        let b1 = blob("one", 10);
        let (a, prev) = db
            .put_asset(NewAsset {
                repo_id: &r.id,
                path: "com/acme/lib/1.0/lib-1.0.jar",
                blob: &b1,
                content_type: "application/java-archive",
                uploader: "admin",
                attributes: serde_json::json!({"k": "v"}),
            })
            .await
            .unwrap();
        assert!(prev.is_none());
        assert_eq!(a.blob.size, 10);
        assert_eq!(a.attributes["k"], "v");

        let b2 = blob("two", 20);
        let (_, prev) = db
            .put_asset(NewAsset {
                repo_id: &r.id,
                path: "com/acme/lib/1.0/lib-1.0.jar",
                blob: &b2,
                content_type: "application/java-archive",
                uploader: "admin",
                attributes: Value::Null,
            })
            .await
            .unwrap();
        assert_eq!(prev.as_deref(), Some(b1.sha256.as_str()));
        db.put_asset(NewAsset {
            repo_id: &r.id,
            path: "com/acme/other.txt",
            blob: &b2,
            content_type: "text/plain",
            uploader: "admin",
            attributes: Value::Null,
        })
        .await
        .unwrap();

        assert_eq!(db.list_assets_under(&r.id, "com/acme/lib").await.unwrap().len(), 1);
        assert_eq!(db.list_assets_under(&r.id, "com/acme").await.unwrap().len(), 2);
        assert_eq!(db.list_assets_under(&r.id, "com/ac").await.unwrap().len(), 0);
        assert_eq!(db.list_assets_under(&r.id, "").await.unwrap().len(), 2);

        let (found, total) =
            db.search_assets(&AssetQuery { q: "LIB-1".into(), limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(found[0].path, "com/acme/lib/1.0/lib-1.0.jar");
        let (_, total) = db
            .search_assets(&AssetQuery {
                q: "acme".into(),
                repo_id: Some(r.id.clone()),
                limit: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 2);
        let (_, total) = db
            .search_assets(&AssetQuery { q: "acme".into(), exclude_repo_ids: vec![r.id.clone()], ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 0);

        let stats = db.repo_stats().await.unwrap();
        assert_eq!(stats[0].asset_count, 2);
        assert_eq!(stats[0].total_size, 40);

        // b1 is now orphaned
        let orphans = db.orphan_blobs(now_millis() + 1, 10).await.unwrap();
        assert_eq!(orphans, vec![b1.sha256.clone()]);
        assert!(db.delete_blob_if_orphan(&b1.sha256).await.unwrap());
        assert!(!db.delete_blob_if_orphan(&b2.sha256).await.unwrap());

        let deleted = db.delete_assets_under(&r.id, "com").await.unwrap();
        assert_eq!(deleted.len(), 2);
        db.delete_repo(&r.id).await.unwrap();
        assert!(db.get_repo(&r.id).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn users_tokens_settings_audit() {
    let _serial = PG_LOCK.lock().await;
    for (db, _guard) in databases().await {
        let u = User {
            id: new_id(),
            username: "alice".into(),
            display_name: "Alice".into(),
            email: "a@example.com".into(),
            role: Role::Deployer,
            disabled: false,
            created_at: now_millis(),
            password_hash: "x".into(),
        };
        db.create_user(&u).await.unwrap();
        assert_eq!(db.count_users().await.unwrap(), 1);
        assert_eq!(db.count_admins().await.unwrap(), 0);

        let t = Token {
            id: new_id(),
            user_id: u.id.clone(),
            name: "ci".into(),
            kind: TokenKind::Api,
            prefix: "trv_abcd".into(),
            created_at: now_millis(),
            expires_at: None,
            last_used_at: None,
        };
        db.create_token(&t, "hash1").await.unwrap();
        let expired = Token { id: new_id(), expires_at: Some(1), ..t.clone() };
        db.create_token(&expired, "hash2").await.unwrap();
        let (tok, user) = db.find_token("hash1").await.unwrap().unwrap();
        assert_eq!(tok.name, "ci");
        assert_eq!(user.username, "alice");
        assert!(db.find_token("hash2").await.unwrap().is_none());
        assert_eq!(db.purge_expired_tokens().await.unwrap(), 1);
        assert_eq!(db.list_tokens(&u.id, TokenKind::Api).await.unwrap().len(), 1);

        db.set_setting("anonymous_read", "true").await.unwrap();
        db.set_setting("anonymous_read", "false").await.unwrap();
        assert_eq!(db.get_setting("anonymous_read").await.unwrap().as_deref(), Some("false"));

        for i in 0..3 {
            db.insert_audit(&AuditEvent {
                id: new_id(),
                ts: now_millis() + i,
                actor: "alice".into(),
                action: format!("asset.{}", if i == 0 { "delete" } else { "upload" }),
                target: "raw/x".into(),
                detail: String::new(),
                client_ip: "127.0.0.1".into(),
            })
            .await
            .unwrap();
        }
        let (events, total) = db
            .list_audit(&AuditQuery { action: Some("asset.up".into()), limit: 10, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(events.len(), 2);

        db.delete_user(&u.id).await.unwrap();
        assert!(db.get_user_by_name("alice").await.unwrap().is_none());
    }
}
