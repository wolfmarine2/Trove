//! End-to-end tests against a real server on a random port (no network needed).

use std::net::SocketAddr;

use base64::Engine;
use reqwest::StatusCode;
use serde_json::{Value, json};
use trove_core::config::Config;

fn hex_sha1(data: &[u8]) -> String {
    use sha1::Digest;
    hex::encode(sha1::Sha1::digest(data))
}

struct Server {
    addr: SocketAddr,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

const ADMIN: (&str, &str) = ("admin", "admin-password-1");

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.path().to_path_buf();
        cfg.security.initial_admin_password = ADMIN.1.into();
        let state = trove_server::init(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, trove_server::router(state)).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        Self { addr, client, _dir: dir }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn admin(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(method, self.url(path)).basic_auth(ADMIN.0, Some(ADMIN.1))
    }

    async fn create_repo(&self, body: Value) {
        let resp = self.admin(reqwest::Method::POST, "/api/v1/repositories").json(&body).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "{}", resp.text().await.unwrap());
    }

    async fn token(&self) -> String {
        let resp =
            self.admin(reqwest::Method::POST, "/api/v1/tokens").json(&json!({"name": "test"})).send().await.unwrap();
        resp.json::<Value>().await.unwrap()["secret"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn raw_hosted_proxy_group() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "files", "format": "raw", "kind": "hosted"})).await;
    let upstream = s.url("/repository/files/");
    s.create_repo(
        json!({"name": "files-proxy", "format": "raw", "kind": "proxy", "config": {"proxy": {"remote_url": upstream}}}),
    )
    .await;
    s.create_repo(json!({"name": "files-all", "format": "raw", "kind": "group", "config": {"group": {"members": ["files-proxy"]}}})).await;

    let put = |path: &str, body: &'static str| s.admin(reqwest::Method::PUT, path).body(body).send();
    assert_eq!(put("/repository/files/a/b.txt", "hello").await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(put("/repository/files/a/b.txt", "again").await.unwrap().status(), StatusCode::CONFLICT);
    let anon = s.client.put(s.url("/repository/files/x")).body("x").send().await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    assert!(anon.headers().contains_key("www-authenticate"));

    for repo in ["files", "files-proxy", "files-all"] {
        let resp = s.client.get(s.url(&format!("/repository/{repo}/a/b.txt"))).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{repo}");
        assert_eq!(resp.text().await.unwrap(), "hello");
    }
    let missing = s.client.get(s.url("/repository/files-all/nope")).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // Proxy cached the file: browse shows it.
    let browse: Value = s
        .client
        .get(s.url("/api/v1/repositories/files-proxy/browse?path=a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(browse["entries"][0]["name"], "b.txt");

    // Conditional GET.
    let first = s.client.get(s.url("/repository/files/a/b.txt")).send().await.unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let second = s.client.get(s.url("/repository/files/a/b.txt")).header("if-none-match", etag).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);

    // Delete (admin) and GC reclaims nothing yet because of the grace period.
    let del = s.admin(reqwest::Method::DELETE, "/repository/files/a").send().await.unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let gone = s.client.get(s.url("/repository/files/a/b.txt")).send().await.unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn maven_checksums_policy_and_group_metadata() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "rel", "format": "maven", "kind": "hosted"})).await;
    s.create_repo(json!({"name": "snap", "format": "maven", "kind": "hosted", "config": {"version_policy": "snapshot", "write_policy": "allow_redeploy"}})).await;
    s.create_repo(json!({"name": "public", "format": "maven", "kind": "group", "config": {"group": {"members": ["rel", "snap"]}}})).await;

    let put = |path: String, body: String| s.admin(reqwest::Method::PUT, &path).body(body).send();
    assert_eq!(
        put("/repository/rel/g/a/1.0/a-1.0.jar".to_string(), "jar".into()).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        put("/repository/rel/g/a/2.0-SNAPSHOT/a-2.0-1.jar".to_string(), "x".into()).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        put("/repository/snap/g/a/2.0-SNAPSHOT/a-2.0-1.jar".to_string(), "x".into()).await.unwrap().status(),
        StatusCode::CREATED
    );

    let md = |v: &str, ts: &str| {
        format!(
            "<metadata><groupId>g</groupId><artifactId>a</artifactId><versioning><versions><version>{v}</version></versions><lastUpdated>{ts}</lastUpdated></versioning></metadata>"
        )
    };
    put("/repository/rel/g/a/maven-metadata.xml".into(), md("1.0", "20240101000000")).await.unwrap();
    put("/repository/rel/g/a/maven-metadata.xml".into(), md("1.0", "20240101000001")).await.unwrap();
    put("/repository/snap/g/a/maven-metadata.xml".into(), md("2.0-SNAPSHOT", "20250101000000")).await.unwrap();

    let sha1 =
        s.client.get(s.url("/repository/public/g/a/1.0/a-1.0.jar.sha1")).send().await.unwrap().text().await.unwrap();
    assert_eq!(sha1, hex_sha1(b"jar"));

    let merged =
        s.client.get(s.url("/repository/public/g/a/maven-metadata.xml")).send().await.unwrap().text().await.unwrap();
    assert!(
        merged.contains("<version>1.0</version>") && merged.contains("<version>2.0-SNAPSHOT</version>"),
        "{merged}"
    );
    assert!(merged.contains("<release>1.0</release>"));
    assert!(merged.contains("<lastUpdated>20250101000000</lastUpdated>"));
}

#[tokio::test]
async fn npm_publish_and_install_metadata() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "npm-local", "format": "npm", "kind": "hosted"})).await;
    s.create_repo(
        json!({"name": "npm-all", "format": "npm", "kind": "group", "config": {"group": {"members": ["npm-local"]}}}),
    )
    .await;
    let token = s.token().await;
    let tgz = b"fake tarball bytes";
    let doc = json!({
        "_id": "@acme/x", "name": "@acme/x", "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "@acme/x", "version": "1.0.0", "dist": {"tarball": "http://ignored/@acme/x/-/@acme/x-1.0.0.tgz"}}},
        "_attachments": {"@acme/x-1.0.0.tgz": {"content_type": "application/octet-stream", "data": base64::engine::general_purpose::STANDARD.encode(tgz), "length": tgz.len()}},
    });
    let publish = || s.client.put(s.url("/repository/npm-local/@acme%2fx")).bearer_auth(&token).json(&doc).send();
    assert_eq!(publish().await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(publish().await.unwrap().status(), StatusCode::FORBIDDEN);

    let p: Value = s.client.get(s.url("/repository/npm-all/@acme%2fx")).send().await.unwrap().json().await.unwrap();
    let tarball = p["versions"]["1.0.0"]["dist"]["tarball"].as_str().unwrap();
    assert_eq!(tarball, s.url("/repository/npm-all/@acme/x/-/x-1.0.0.tgz"));
    assert!(p["versions"]["1.0.0"]["dist"]["integrity"].as_str().unwrap().starts_with("sha512-"));
    let body = s.client.get(tarball).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(&body[..], tgz);

    let who: Value = s
        .client
        .get(s.url("/repository/npm-local/-/whoami"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["username"], "admin");
}

#[tokio::test]
async fn pypi_upload_and_simple_index() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "py", "format": "pypi", "kind": "hosted"})).await;
    let form = reqwest::multipart::Form::new()
        .text(":action", "file_upload")
        .text("name", "My_Pkg")
        .text("version", "1.0")
        .text("requires_python", ">=3.9")
        .part("content", reqwest::multipart::Part::bytes(b"wheel".to_vec()).file_name("my_pkg-1.0-py3-none-any.whl"));
    let resp = s.admin(reqwest::Method::POST, "/repository/py/").multipart(form).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text().await.unwrap());

    let html = s.client.get(s.url("/repository/py/simple/my-pkg/")).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("my_pkg-1.0-py3-none-any.whl#sha256="), "{html}");
    assert!(html.contains("data-requires-python=\"&gt;=3.9\""));
    let file = s.client.get(s.url("/repository/py/packages/my-pkg/my_pkg-1.0-py3-none-any.whl")).send().await.unwrap();
    assert_eq!(file.bytes().await.unwrap().as_ref(), b"wheel");
    let root = s.client.get(s.url("/repository/py/simple/")).send().await.unwrap().text().await.unwrap();
    assert!(root.contains("my-pkg"));
}

#[tokio::test]
async fn cargo_publish_index_download() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "crates", "format": "cargo", "kind": "hosted"})).await;
    let token = s.token().await;
    let meta = serde_json::to_vec(&json!({"name": "demo", "vers": "0.1.0", "deps": [], "features": {}, "links": null}))
        .unwrap();
    let crate_bytes = b"crate-archive";
    let mut body = Vec::new();
    body.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    body.extend_from_slice(&meta);
    body.extend_from_slice(&(crate_bytes.len() as u32).to_le_bytes());
    body.extend_from_slice(crate_bytes);
    let publish = || {
        s.client
            .put(s.url("/repository/crates/api/v1/crates/new"))
            .header("authorization", &token)
            .body(body.clone())
            .send()
    };
    assert_eq!(publish().await.unwrap().status(), StatusCode::OK);
    let dup = publish().await.unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    assert!(dup.json::<Value>().await.unwrap()["errors"][0]["detail"].as_str().unwrap().contains("already"));

    let cfg: Value =
        s.client.get(s.url("/repository/crates/index/config.json")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["dl"], s.url("/repository/crates/api/v1/crates"));
    let index = s.client.get(s.url("/repository/crates/index/de/mo/demo")).send().await.unwrap().text().await.unwrap();
    let entry: Value = serde_json::from_str(index.trim()).unwrap();
    assert_eq!(entry["vers"], "0.1.0");
    let dl = s
        .client
        .get(s.url("/repository/crates/api/v1/crates/demo/0.1.0/download"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(dl.as_ref(), crate_bytes);
    assert_eq!(entry["cksum"], trove_core::util::sha256_hex(crate_bytes));

    let unauth = s.client.put(s.url("/repository/crates/api/v1/crates/new")).body(body.clone()).send().await.unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_api_permissions() {
    let s = Server::start().await;
    let resp = s
        .admin(reqwest::Method::POST, "/api/v1/users")
        .json(&json!({"username": "viewer1", "password": "viewer-pass-1", "role": "viewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let as_viewer = s
        .client
        .post(s.url("/api/v1/repositories"))
        .basic_auth("viewer1", Some("viewer-pass-1"))
        .json(&json!({"name": "x", "format": "raw", "kind": "hosted"}))
        .send()
        .await
        .unwrap();
    assert_eq!(as_viewer.status(), StatusCode::FORBIDDEN);
    let bad = s.client.get(s.url("/api/v1/auth/me")).basic_auth("viewer1", Some("wrong")).send().await.unwrap();
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

    // Cannot remove the last administrator.
    let users: Vec<Value> = s.admin(reqwest::Method::GET, "/api/v1/users").send().await.unwrap().json().await.unwrap();
    let admin_id = users.iter().find(|u| u["username"] == "admin").unwrap()["id"].as_str().unwrap().to_string();
    let demote = s
        .admin(reqwest::Method::PUT, &format!("/api/v1/users/{admin_id}"))
        .json(&json!({"role": "viewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(demote.status(), StatusCode::BAD_REQUEST);

    // Audit trail recorded the user creation.
    let audit: Value =
        s.admin(reqwest::Method::GET, "/api/v1/system/audit?action=user.").send().await.unwrap().json().await.unwrap();
    assert_eq!(audit["items"][0]["action"], "user.create");

    let metrics = s.client.get(s.url("/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("trove_auth_failures_total 1"));
}
