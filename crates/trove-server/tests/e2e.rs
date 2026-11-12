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

// ---------------------------------------------------------------- docker

mod mock_registry {
    //! A tiny Docker-Hub-like upstream: bearer-token auth, a multi-arch index,
    //! one image manifest, a config and a layer.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use serde_json::json;

    pub struct Image {
        pub index: Vec<u8>,
        pub manifest: Vec<u8>,
        pub config: Vec<u8>,
        pub layer: Vec<u8>,
    }

    pub fn digest(b: &[u8]) -> String {
        format!("sha256:{}", trove_core::util::sha256_hex(b))
    }

    impl Image {
        pub fn new() -> Self {
            let config = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
            let layer = (0..200_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>();
            let manifest = serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(&config), "size": config.len()},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": digest(&layer), "size": layer.len()}]
            }))
            .unwrap();
            let index = serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest(&manifest), "size": manifest.len(),
                               "platform": {"architecture": "amd64", "os": "linux"}}]
            }))
            .unwrap();
            Self { index, manifest, config, layer }
        }
    }

    pub struct Upstream {
        pub image: Image,
        pub blob_hits: AtomicUsize,
        pub token_hits: AtomicUsize,
        pub port: u16,
    }

    fn challenge(port: u16) -> Response {
        let value = format!(
            r#"Bearer realm="http://127.0.0.1:{port}/token",service="mock",scope="repository:library/alpine:pull""#
        );
        (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, value)], "{}").into_response()
    }

    fn authorized(h: &HeaderMap) -> bool {
        h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some("Bearer tok-123")
    }

    async fn token(State(u): State<Arc<Upstream>>, h: HeaderMap, q: axum::extract::RawQuery) -> Response {
        u.token_hits.fetch_add(1, Ordering::SeqCst);
        let q = q.0.unwrap_or_default();
        assert!(q.contains("service=mock") && q.contains("scope=repository"), "{q}");
        // upstream credentials are forwarded as Basic to the token endpoint
        let basic = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        if basic != "Basic dXNlcjpwYXNz" {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({"token": "tok-123", "expires_in": 300})).into_response()
    }

    async fn manifests(
        State(u): State<Arc<Upstream>>,
        Path((ns, name, reference)): Path<(String, String, String)>,
        h: HeaderMap,
    ) -> Response {
        if !authorized(&h) {
            return challenge(u.port);
        }
        if ns != "library" || name != "alpine" {
            return StatusCode::NOT_FOUND.into_response();
        }
        let img = &u.image;
        let (body, ct) = if reference == "3.20" || reference == digest(&img.index) {
            (img.index.clone(), "application/vnd.oci.image.index.v1+json")
        } else if reference == digest(&img.manifest) {
            (img.manifest.clone(), "application/vnd.oci.image.manifest.v1+json")
        } else if reference == "tampered" {
            (b"{}".to_vec(), "application/vnd.oci.image.manifest.v1+json")
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };
        ([(header::CONTENT_TYPE, ct), (header::HeaderName::from_static("docker-content-digest"), &digest(&body))], body)
            .into_response()
    }

    async fn blobs(
        State(u): State<Arc<Upstream>>,
        Path((_ns, _name, d)): Path<(String, String, String)>,
        h: HeaderMap,
    ) -> Response {
        if !authorized(&h) {
            return challenge(u.port);
        }
        u.blob_hits.fetch_add(1, Ordering::SeqCst);
        let img = &u.image;
        if d == digest(&img.config) {
            img.config.clone().into_response()
        } else if d == digest(&img.layer) {
            img.layer.clone().into_response()
        } else if d == format!("sha256:{}", "e".repeat(64)) {
            b"bytes that do not match".to_vec().into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    }

    async fn tags(State(u): State<Arc<Upstream>>, h: HeaderMap) -> Response {
        if !authorized(&h) {
            return challenge(u.port);
        }
        axum::Json(json!({"name": "library/alpine", "tags": ["3.20", "latest"]})).into_response()
    }

    pub async fn start() -> Arc<Upstream> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let up = Arc::new(Upstream {
            image: Image::new(),
            blob_hits: AtomicUsize::new(0),
            token_hits: AtomicUsize::new(0),
            port,
        });
        let app = Router::new()
            .route("/token", get(token))
            .route("/v2/{ns}/{name}/manifests/{reference}", get(manifests))
            .route("/v2/{ns}/{name}/blobs/{digest}", get(blobs))
            .route("/v2/{ns}/{name}/tags/list", get(tags))
            .with_state(up.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        up
    }
}

#[tokio::test]
async fn docker_proxy_group_and_token_auth() {
    use mock_registry::digest;
    use std::sync::atomic::Ordering;

    let up = mock_registry::start().await;
    let s = Server::start().await;
    let remote = format!("http://127.0.0.1:{}", up.port);
    s.create_repo(json!({"name": "hub", "format": "docker", "kind": "proxy",
        "config": {"proxy": {"remote_url": remote, "username": "user", "password": "pass"}}}))
        .await;
    s.create_repo(json!({"name": "hub2", "format": "docker", "kind": "proxy",
        "config": {"proxy": {"remote_url": format!("{remote}/v2/"), "username": "user", "password": "pass"}}}))
        .await;
    s.create_repo(
        json!({"name": "docker-all", "format": "docker", "kind": "group", "config": {"group": {"members": ["hub"]}}}),
    )
    .await;
    let hosted = s
        .admin(reqwest::Method::POST, "/api/v1/repositories")
        .json(&json!({"name": "dh", "format": "docker", "kind": "hosted"}))
        .send()
        .await
        .unwrap();
    assert_eq!(hosted.status(), StatusCode::BAD_REQUEST);

    let base = s.client.get(s.url("/v2/")).send().await.unwrap();
    assert_eq!(base.status(), StatusCode::OK);
    assert_eq!(base.headers()["docker-distribution-api-version"], "registry/2.0");

    // Resolve the tag the way containerd does: HEAD, then GET by digest.
    let accept = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json";
    let head =
        s.client.head(s.url("/v2/hub/library/alpine/manifests/3.20")).header("accept", accept).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK, "{:?}", head.headers());
    assert_eq!(head.headers()["docker-content-digest"].to_str().unwrap(), digest(&up.image.index));
    assert_eq!(head.headers()["content-type"], "application/vnd.oci.image.index.v1+json");
    assert_eq!(up.token_hits.load(Ordering::SeqCst), 1);

    let index = s
        .client
        .get(s.url(&format!("/v2/docker-all/library/alpine/manifests/{}", digest(&up.image.index))))
        .send()
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(index.bytes().await.unwrap().as_ref(), up.image.index.as_slice());
    let manifest = s
        .client
        .get(s.url(&format!("/v2/docker-all/library/alpine/manifests/{}", digest(&up.image.manifest))))
        .send()
        .await
        .unwrap();
    assert_eq!(manifest.headers()["content-type"], "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(manifest.bytes().await.unwrap().as_ref(), up.image.manifest.as_slice());

    // Layers: first pull downloads, second is served from cache.
    let layer_url = s.url(&format!("/v2/docker-all/library/alpine/blobs/{}", digest(&up.image.layer)));
    for _ in 0..2 {
        let r = s.client.get(&layer_url).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["docker-content-digest"].to_str().unwrap(), digest(&up.image.layer));
        assert_eq!(r.bytes().await.unwrap().len(), up.image.layer.len());
    }
    assert_eq!(up.blob_hits.load(Ordering::SeqCst), 1);
    // Resumed download.
    let part = s.client.get(&layer_url).header("range", "bytes=100-199").send().await.unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.bytes().await.unwrap().as_ref(), &up.image.layer[100..200]);
    // Same layer through another proxy repository is deduplicated (no upstream hit).
    let r = s
        .client
        .get(s.url(&format!("/v2/hub2/library/alpine/blobs/{}", digest(&up.image.layer))))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(up.blob_hits.load(Ordering::SeqCst), 1);

    // Upstream content that does not match its digest is rejected and not cached.
    let bad =
        s.client.get(s.url(&format!("/v2/hub/library/alpine/blobs/sha256:{}", "e".repeat(64)))).send().await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_GATEWAY);
    let unknown = s.client.get(s.url("/v2/hub/library/alpine/manifests/9.99")).send().await.unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let err: Value = unknown.json().await.unwrap();
    assert_eq!(err["errors"][0]["code"], "MANIFEST_UNKNOWN");

    let tags: Value =
        s.client.get(s.url("/v2/docker-all/library/alpine/tags/list")).send().await.unwrap().json().await.unwrap();
    assert_eq!(tags["name"], "docker-all/library/alpine");
    assert_eq!(tags["tags"], json!(["3.20", "latest"]));

    // Default repository: bare names (docker registry-mirrors) route to it.
    let missing = s.client.get(s.url("/v2/library/alpine/manifests/3.20")).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    s.admin(reqwest::Method::PUT, "/api/v1/system/settings")
        .json(&json!({"docker_default_repository": "docker-all"}))
        .send()
        .await
        .unwrap();
    let mirrored = s.client.get(s.url("/v2/library/alpine/manifests/3.20")).send().await.unwrap();
    assert_eq!(mirrored.status(), StatusCode::OK);

    // Pushing is not supported.
    let push = s.admin(reqwest::Method::POST, "/v2/hub/library/alpine/blobs/uploads/").send().await.unwrap();
    assert_eq!(push.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Private mode: anonymous clients get a Basic challenge, docker login works.
    s.admin(reqwest::Method::PUT, "/api/v1/system/settings")
        .json(&json!({"anonymous_read": false}))
        .send()
        .await
        .unwrap();
    let anon = s.client.get(s.url("/v2/")).send().await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    assert!(anon.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    let login = s.client.get(s.url("/v2/")).basic_auth(ADMIN.0, Some(ADMIN.1)).send().await.unwrap();
    assert_eq!(login.status(), StatusCode::OK);
}

// ---------------------------------------------------------------- yum / apt

#[tokio::test]
async fn yum_and_apt_proxies_refresh_metadata() {
    let s = Server::start().await;
    // Upstream: a raw hosted repository on the same server, with redeploys allowed.
    s.create_repo(
        json!({"name": "upstream", "format": "raw", "kind": "hosted", "config": {"write_policy": "allow_redeploy"}}),
    )
    .await;
    let put = |path: &str, body: &str| {
        s.admin(reqwest::Method::PUT, &format!("/repository/upstream/{path}")).body(body.to_string()).send()
    };
    put("el/repodata/repomd.xml", "<repomd>v1</repomd>").await.unwrap();
    put("el/repodata/primary.xml.gz", "primary-v1").await.unwrap();
    put("el/Packages/a-1.0.rpm", "rpm-bytes").await.unwrap();
    put("deb/dists/noble/InRelease", "release-v1").await.unwrap();
    put("deb/dists/noble/main/binary-amd64/Packages", "packages-v1").await.unwrap();
    put("deb/pool/main/a/a_1.0_amd64.deb", "deb-bytes").await.unwrap();

    let upstream = s.url("/repository/upstream");
    // max age of one second so the test can observe refreshes
    s.create_repo(json!({"name": "el", "format": "yum", "kind": "proxy",
        "config": {"proxy": {"remote_url": format!("{upstream}/el/"), "metadata_max_age_secs": 1}}}))
        .await;
    s.create_repo(json!({"name": "ubuntu", "format": "apt", "kind": "proxy",
        "config": {"proxy": {"remote_url": format!("{upstream}/deb/"), "metadata_max_age_secs": 1}}}))
        .await;
    let group = s
        .admin(reqwest::Method::POST, "/api/v1/repositories")
        .json(&json!({"name": "g", "format": "yum", "kind": "group", "config": {"group": {"members": ["el"]}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(group.status(), StatusCode::BAD_REQUEST);

    let s = &s;
    let get = |path: String| async move { s.client.get(s.url(&path)).send().await.unwrap().text().await.unwrap() };
    assert_eq!(get("/repository/el/repodata/repomd.xml".into()).await, "<repomd>v1</repomd>");
    assert_eq!(get("/repository/el/repodata/primary.xml.gz".into()).await, "primary-v1");
    assert_eq!(get("/repository/el/Packages/a-1.0.rpm".into()).await, "rpm-bytes");
    assert_eq!(get("/repository/ubuntu/dists/noble/main/binary-amd64/Packages".into()).await, "packages-v1");
    assert_eq!(get("/repository/ubuntu/pool/main/a/a_1.0_amd64.deb".into()).await, "deb-bytes");

    // Upstream publishes new metadata; packages are immutable and keep being served.
    put("el/repodata/repomd.xml", "<repomd>v2</repomd>").await.unwrap();
    put("el/repodata/primary.xml.gz", "primary-v2").await.unwrap();
    put("el/Packages/a-1.0.rpm", "changed-upstream").await.unwrap();
    put("deb/dists/noble/InRelease", "release-v2").await.unwrap();
    put("deb/dists/noble/main/binary-amd64/Packages", "packages-v2").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    assert_eq!(get("/repository/el/repodata/repomd.xml".into()).await, "<repomd>v2</repomd>");
    assert_eq!(get("/repository/el/repodata/primary.xml.gz".into()).await, "primary-v2");
    assert_eq!(get("/repository/el/Packages/a-1.0.rpm".into()).await, "rpm-bytes");
    assert_eq!(get("/repository/ubuntu/dists/noble/main/binary-amd64/Packages".into()).await, "packages-v2");
    assert_eq!(get("/repository/ubuntu/dists/noble/InRelease".into()).await, "release-v2");

    // If-Modified-Since (apt) and read-only behaviour.
    let lm = s.client.get(s.url("/repository/ubuntu/dists/noble/InRelease")).send().await.unwrap().headers()["last-modified"].clone();
    let cond = s
        .client
        .get(s.url("/repository/ubuntu/dists/noble/InRelease"))
        .header("if-modified-since", lm)
        .send()
        .await
        .unwrap();
    assert_eq!(cond.status(), StatusCode::NOT_MODIFIED);
    let write = s.admin(reqwest::Method::PUT, "/repository/el/x.rpm").body("x").send().await.unwrap();
    assert_eq!(write.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------- outbound proxies

mod mini_proxies {
    //! Minimal HTTP (absolute-form) and SOCKS5 (RFC 1928/1929) proxies that
    //! count the connections they relay.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Default)]
    pub struct Stats {
        pub relayed: AtomicUsize,
        pub rejected: AtomicUsize,
        pub saw_domain: AtomicBool,
    }

    async fn pipe(a: TcpStream, b: TcpStream) {
        let (mut ar, mut aw) = a.into_split();
        let (mut br, mut bw) = b.into_split();
        tokio::select! {
            _ = tokio::io::copy(&mut ar, &mut bw) => {}
            _ = tokio::io::copy(&mut br, &mut aw) => {}
        }
    }

    pub async fn http(user: &'static str, pass: &'static str) -> (u16, Arc<Stats>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stats = Arc::new(Stats::default());
        let st = stats.clone();
        let expected = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")));
        tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let (st, expected) = (st.clone(), expected.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = client.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let headers: Vec<&str> = lines.take_while(|l| !l.is_empty()).collect();
                    let auth_ok = headers.iter().any(|h| {
                        h.to_ascii_lowercase().starts_with("proxy-authorization:") && h[20..].trim() == expected
                    });
                    if !auth_ok {
                        st.rejected.fetch_add(1, Ordering::SeqCst);
                        let _ = client
                            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"p\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let mut parts = request_line.split(' ');
                    let (method, target) = (parts.next().unwrap_or("GET"), parts.next().unwrap_or("/"));
                    let url = url::Url::parse(target).unwrap();
                    let mut upstream =
                        TcpStream::connect((url.host_str().unwrap(), url.port_or_known_default().unwrap()))
                            .await
                            .unwrap();
                    let path = match url.query() {
                        Some(q) => format!("{}?{q}", url.path()),
                        None => url.path().to_string(),
                    };
                    let mut out = format!("{method} {path} HTTP/1.1\r\n");
                    for h in &headers {
                        let lower = h.to_ascii_lowercase();
                        if !lower.starts_with("proxy-") && !lower.starts_with("connection:") {
                            out.push_str(h);
                            out.push_str("\r\n");
                        }
                    }
                    out.push_str("Connection: close\r\n\r\n");
                    upstream.write_all(out.as_bytes()).await.unwrap();
                    st.relayed.fetch_add(1, Ordering::SeqCst);
                    pipe(client, upstream).await;
                });
            }
        });
        (port, stats)
    }

    pub async fn socks5(user: &'static str, pass: &'static str) -> (u16, Arc<Stats>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stats = Arc::new(Stats::default());
        let st = stats.clone();
        tokio::spawn(async move {
            loop {
                let (mut c, _) = listener.accept().await.unwrap();
                let st = st.clone();
                tokio::spawn(async move {
                    let mut hdr = [0u8; 2];
                    c.read_exact(&mut hdr).await.unwrap();
                    let mut methods = vec![0u8; hdr[1] as usize];
                    c.read_exact(&mut methods).await.unwrap();
                    if !methods.contains(&2) {
                        st.rejected.fetch_add(1, Ordering::SeqCst);
                        let _ = c.write_all(&[5, 0xff]).await;
                        return;
                    }
                    c.write_all(&[5, 2]).await.unwrap();
                    // RFC 1929 username/password
                    let mut v = [0u8; 2];
                    c.read_exact(&mut v).await.unwrap();
                    let mut u = vec![0u8; v[1] as usize];
                    c.read_exact(&mut u).await.unwrap();
                    let mut pl = [0u8; 1];
                    c.read_exact(&mut pl).await.unwrap();
                    let mut p = vec![0u8; pl[0] as usize];
                    c.read_exact(&mut p).await.unwrap();
                    if u != user.as_bytes() || p != pass.as_bytes() {
                        st.rejected.fetch_add(1, Ordering::SeqCst);
                        let _ = c.write_all(&[1, 1]).await;
                        return;
                    }
                    c.write_all(&[1, 0]).await.unwrap();
                    let mut req = [0u8; 4];
                    c.read_exact(&mut req).await.unwrap();
                    let host = match req[3] {
                        1 => {
                            let mut a = [0u8; 4];
                            c.read_exact(&mut a).await.unwrap();
                            std::net::Ipv4Addr::from(a).to_string()
                        }
                        3 => {
                            let mut l = [0u8; 1];
                            c.read_exact(&mut l).await.unwrap();
                            let mut name = vec![0u8; l[0] as usize];
                            c.read_exact(&mut name).await.unwrap();
                            st.saw_domain.store(true, Ordering::SeqCst);
                            String::from_utf8(name).unwrap()
                        }
                        _ => return,
                    };
                    let mut pb = [0u8; 2];
                    c.read_exact(&mut pb).await.unwrap();
                    let upstream = TcpStream::connect((host.as_str(), u16::from_be_bytes(pb))).await.unwrap();
                    c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                    st.relayed.fetch_add(1, Ordering::SeqCst);
                    pipe(c, upstream).await;
                });
            }
        });
        (port, stats)
    }
}

#[tokio::test]
async fn per_repository_outbound_proxies() {
    use std::sync::atomic::Ordering;

    let s = Server::start().await;
    let (http_port, http_stats) = mini_proxies::http("puser", "ppass").await;
    let (socks_port, socks_stats) = mini_proxies::socks5("suser", "spass").await;
    s.create_repo(json!({"name": "origin", "format": "raw", "kind": "hosted"})).await;
    for f in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
        s.admin(reqwest::Method::PUT, &format!("/repository/origin/{f}"))
            .body(format!("content of {f}"))
            .send()
            .await
            .unwrap();
    }
    let port = s.addr.port();
    let origin_ip = format!("http://127.0.0.1:{port}/repository/origin/");
    let origin_name = format!("http://localhost:{port}/repository/origin/");

    let mk = |name: &str, remote: &str, outbound: Value| json!({"name": name, "format": "raw", "kind": "proxy", "config": {"proxy": {"remote_url": remote, "outbound": outbound}}});
    s.create_repo(mk("via-http", &origin_ip, json!({"mode": "custom", "url": format!("http://127.0.0.1:{http_port}"), "username": "puser", "password": "ppass"}))).await;
    s.create_repo(mk("via-socks", &origin_name, json!({"mode": "custom", "url": format!("socks5h://127.0.0.1:{socks_port}"), "username": "suser", "password": "spass"}))).await;
    s.create_repo(mk("bad-creds", &origin_ip, json!({"mode": "custom", "url": format!("http://127.0.0.1:{http_port}"), "username": "puser", "password": "wrong"}))).await;
    s.create_repo(mk("direct", &origin_ip, json!({"mode": "direct"}))).await;

    let get = |path: &str| s.client.get(s.url(path)).send();
    let r = get("/repository/via-http/a.txt").await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.text().await.unwrap(), "content of a.txt");
    assert_eq!(http_stats.relayed.load(Ordering::SeqCst), 1);

    let r = get("/repository/via-socks/b.txt").await.unwrap();
    assert_eq!(r.text().await.unwrap(), "content of b.txt");
    assert_eq!(socks_stats.relayed.load(Ordering::SeqCst), 1);
    assert!(socks_stats.saw_domain.load(Ordering::SeqCst), "socks5h must let the proxy resolve host names");

    let r = get("/repository/bad-creds/c.txt").await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert!(http_stats.rejected.load(Ordering::SeqCst) >= 1);

    let r = get("/repository/direct/d.txt").await.unwrap();
    assert_eq!(r.text().await.unwrap(), "content of d.txt");
    assert_eq!(http_stats.relayed.load(Ordering::SeqCst) + socks_stats.relayed.load(Ordering::SeqCst), 2);

    // Secrets are masked in the API and kept when the masked value is sent back.
    let repo: Value =
        s.admin(reqwest::Method::GET, "/api/v1/repositories/via-socks").send().await.unwrap().json().await.unwrap();
    assert_eq!(repo["config"]["proxy"]["outbound"]["password"], "********");
    let mut cfg = repo["config"].clone();
    cfg["proxy"]["metadata_max_age_secs"] = json!(60);
    let upd = s
        .admin(reqwest::Method::PUT, "/api/v1/repositories/via-socks")
        .json(&json!({"config": cfg}))
        .send()
        .await
        .unwrap();
    assert_eq!(upd.status(), StatusCode::OK);
    // Had the mask been stored as the password, the new client would fail SOCKS auth.
    let r = get("/repository/via-socks/e.txt").await.unwrap();
    assert_eq!(r.text().await.unwrap(), "content of e.txt");
    assert_eq!(socks_stats.rejected.load(Ordering::SeqCst), 0);

    // Connection test without saving.
    let test = |body: Value| s.admin(reqwest::Method::POST, "/api/v1/remote-test").json(&body).send();
    let ok: Value = test(json!({"format": "raw", "name": "via-socks", "proxy": repo["config"]["proxy"]}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    let bad: Value = test(json!({"format": "raw", "proxy": {"remote_url": origin_ip,
        "outbound": {"mode": "custom", "url": format!("http://127.0.0.1:{http_port}"), "username": "x", "password": "y"}}}))
        .await.unwrap().json().await.unwrap();
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["status"], 407);
    let down: Value = test(json!({"format": "raw", "proxy": {"remote_url": origin_ip,
        "outbound": {"mode": "custom", "url": "socks5://127.0.0.1:1"}}}))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(down["ok"], false);
    assert!(down["status"].is_null());

    // Validation.
    let invalid = s
        .admin(reqwest::Method::POST, "/api/v1/repositories")
        .json(&mk("bad-url", &origin_ip, json!({"mode": "custom", "url": "ftp://proxy:21"})))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn fresh_install_is_an_anonymous_public_mirror() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "files", "format": "raw", "kind": "hosted"})).await;
    s.create_repo(json!({"name": "crates", "format": "cargo", "kind": "hosted"})).await;
    s.admin(reqwest::Method::PUT, "/repository/files/tool.tar.gz").body("tool").send().await.unwrap();

    // Everything readable without credentials.
    let anon = |path: &str| s.client.get(s.url(path)).send();
    assert_eq!(anon("/api/v1/status").await.unwrap().json::<Value>().await.unwrap()["anonymous_read"], true);
    for path in [
        "/repository/files/tool.tar.gz",
        "/api/v1/repositories",
        "/api/v1/repositories/files/browse?path=",
        "/api/v1/assets?q=tool",
        "/api/v1/stats",
        "/v2/",
    ] {
        assert_eq!(anon(path).await.unwrap().status(), StatusCode::OK, "{path}");
    }
    let cfg: Value = anon("/repository/crates/index/config.json").await.unwrap().json().await.unwrap();
    assert_eq!(cfg["auth-required"], false);

    // Writes and administration still need a user.
    let put = s.client.put(s.url("/repository/files/x")).body("x").send().await.unwrap();
    assert_eq!(put.status(), StatusCode::UNAUTHORIZED);
    let del = s.client.delete(s.url("/repository/files/tool.tar.gz")).send().await.unwrap();
    assert_eq!(del.status(), StatusCode::UNAUTHORIZED);
    let create = s
        .client
        .post(s.url("/api/v1/repositories"))
        .json(&json!({"name": "x", "format": "raw", "kind": "hosted"}))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(anon("/api/v1/users").await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(anon("/api/v1/system/audit").await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn raw_private_repositories_uploads_and_disposition() {
    let s = Server::start().await;
    s.create_repo(json!({"name": "certs", "format": "raw", "kind": "hosted", "config": {"private": true}})).await;
    s.create_repo(
        json!({"name": "site", "format": "raw", "kind": "hosted", "config": {"content_disposition": "inline"}}),
    )
    .await;
    s.create_repo(json!({"name": "files", "format": "raw", "kind": "hosted"})).await;

    // Browser session uploads need the CSRF header.
    let jar = reqwest::Client::builder().no_proxy().cookie_store(true).build().unwrap();
    let login = jar
        .post(s.url("/api/v1/auth/login"))
        .json(&json!({"username": ADMIN.0, "password": ADMIN.1}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let no_csrf =
        jar.put(s.url("/repository/certs/ca/root-ca.crt")).body("-----BEGIN CERTIFICATE-----").send().await.unwrap();
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);
    for (path, body) in [
        ("ca/root-ca.crt", "-----BEGIN CERTIFICATE-----"),
        ("ca/intermediate.crt", "INT"),
        ("keys/server.key", "PRIVATE"),
    ] {
        let r = jar
            .put(s.url(&format!("/repository/certs/{path}")))
            .header("x-trove-request", "1")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "{path}");
    }

    // Anonymous callers cannot see or read the private repository...
    let anon = |p: &str| s.client.get(s.url(p)).send();
    let r = anon("/repository/certs/ca/root-ca.crt").await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(r.headers().contains_key("www-authenticate"));
    let repos: Vec<Value> = anon("/api/v1/repositories").await.unwrap().json().await.unwrap();
    assert!(repos.iter().all(|r| r["name"] != "certs"));
    assert_eq!(anon("/api/v1/repositories/certs/browse?path=").await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let found: Value = anon("/api/v1/assets?q=.crt").await.unwrap().json().await.unwrap();
    assert_eq!(found["total"], 0);
    // ...signed-in users can, and downloads are attachments with the file name.
    let r = s.admin(reqwest::Method::GET, "/repository/certs/ca/root-ca.crt").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let cd = r.headers()["content-disposition"].to_str().unwrap().to_string();
    assert!(cd.starts_with("attachment; filename=\"root-ca.crt\""), "{cd}");
    let found: Value =
        s.admin(reqwest::Method::GET, "/api/v1/assets?q=.crt").send().await.unwrap().json().await.unwrap();
    assert_eq!(found["total"], 2);

    // Public repositories stay anonymous; inline files are sandboxed.
    s.admin(reqwest::Method::PUT, "/repository/site/index.html")
        .body("<script>alert(1)</script>")
        .send()
        .await
        .unwrap();
    let r = anon("/repository/site/index.html").await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-disposition"].to_str().unwrap().starts_with("inline"));
    assert!(r.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");

    // A public group must not expose a private member.
    let bad = s.admin(reqwest::Method::POST, "/api/v1/repositories")
        .json(&json!({"name": "all-raw", "format": "raw", "kind": "group", "config": {"group": {"members": ["files", "certs"]}}}))
        .send().await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    s.create_repo(
        json!({"name": "all-raw", "format": "raw", "kind": "group", "config": {"group": {"members": ["files"]}}}),
    )
    .await;
    let files: Value =
        s.admin(reqwest::Method::GET, "/api/v1/repositories/files").send().await.unwrap().json().await.unwrap();
    let mut cfg = files["config"].clone();
    cfg["private"] = json!(true);
    let r =
        s.admin(reqwest::Method::PUT, "/api/v1/repositories/files").json(&json!({"config": cfg})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "files is a member of the public group all-raw");
    s.create_repo(json!({"name": "all-private", "format": "raw", "kind": "group",
        "config": {"private": true, "group": {"members": ["certs"]}}}))
        .await;
    assert_eq!(anon("/repository/all-private/ca/root-ca.crt").await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        s.admin(reqwest::Method::GET, "/repository/all-private/ca/root-ca.crt").send().await.unwrap().status(),
        StatusCode::OK
    );

    // Directory delete through the browser session.
    let del = jar.delete(s.url("/repository/certs/keys")).header("x-trove-request", "1").send().await.unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        s.admin(reqwest::Method::GET, "/repository/certs/keys/server.key").send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // Private Cargo registries tell Cargo to send its token.
    s.create_repo(json!({"name": "crates-priv", "format": "cargo", "kind": "hosted", "config": {"private": true}}))
        .await;
    let cfg: Value = anon("/repository/crates-priv/index/config.json").await.unwrap().json().await.unwrap();
    assert_eq!(cfg["auth-required"], true);
}

/// Run a git command isolated from the developer's configuration and proxies.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .output()
        .expect("git is installed");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Server {
    /// Publish a bare repository as a dumb-HTTP Git remote in a raw repository.
    async fn publish_dumb_git(&self, raw_repo: &str, bare: &std::path::Path, name: &str) {
        git(bare, &["update-server-info"]);
        let mut stack = vec![bare.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let rel = p.strip_prefix(bare).unwrap().to_string_lossy().replace('\\', "/");
                let r = self
                    .admin(reqwest::Method::PUT, &format!("/repository/{raw_repo}/{name}.git/{rel}"))
                    .body(std::fs::read(&p).unwrap())
                    .send()
                    .await
                    .unwrap();
                assert!(r.status().is_success(), "{rel}: {}", r.status());
            }
        }
    }
}

// Multi-threaded: the test drives the synchronous `git` client while the server runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_proxy_mirrors_clones_archives_and_raw_files() {
    let s = Server::start().await;
    s.create_repo(
        json!({"name": "forge", "format": "raw", "kind": "hosted", "config": {"write_policy": "allow_redeploy"}}),
    )
    .await;
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("README.md"), "v1\n").unwrap();
    std::fs::create_dir_all(src.join(".claude-plugin")).unwrap();
    std::fs::write(src.join(".claude-plugin/marketplace.json"), r#"{"name":"m","plugins":[]}"#).unwrap();
    std::fs::write(src.join("package.json"), r#"{"name":"dsh-plugin-demo","version":"1.0.0"}"#).unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "one"]);
    git(&src, &["tag", "v1.0.0"]);
    let bare = work.path().join("plugin.git");
    git(work.path(), &["clone", "-q", "--bare", src.to_str().unwrap(), bare.to_str().unwrap()]);
    s.publish_dumb_git("forge", &bare, "acme/plugin").await;

    s.create_repo(json!({"name": "github", "format": "git", "kind": "proxy", "config": {"proxy": {
        "remote_url": s.url("/repository/forge/"), "metadata_max_age_secs": 1,
        "outbound": {"mode": "direct"}}}}))
        .await;
    let base = s.url("/repository/github");

    // Clone through the proxy (smart HTTP served from the mirror).
    let clone = work.path().join("clone");
    git(work.path(), &["clone", "-q", &format!("{base}/acme/plugin.git"), clone.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(clone.join("README.md")).unwrap(), "v1\n");
    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
    assert!(git(&clone, &["tag"]).contains("v1.0.0"));
    // url.insteadOf rewriting, as clients configure it for github.com.
    let shallow = work.path().join("shallow");
    git(
        work.path(),
        &[
            "-c",
            &format!("url.{base}/.insteadOf=https://github.example/"),
            "clone",
            "-q",
            "--depth",
            "1",
            "https://github.example/acme/plugin",
            shallow.to_str().unwrap(),
        ],
    );
    assert!(shallow.join(".claude-plugin/marketplace.json").exists());

    // Raw files and source archives.
    let get = |p: String| s.client.get(format!("{base}/{p}")).send();
    let r = get("acme/plugin/raw/main/.claude-plugin/marketplace.json".into()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(r.text().await.unwrap(), r#"{"name":"m","plugins":[]}"#);
    assert_eq!(get("acme/plugin/raw/main/missing.txt".into()).await.unwrap().status(), StatusCode::NOT_FOUND);
    let tgz = get("acme/plugin/archive/v1.0.0.tar.gz".into()).await.unwrap();
    assert_eq!(tgz.status(), StatusCode::OK);
    assert_eq!(tgz.headers()["content-type"], "application/gzip");
    let bytes = tgz.bytes().await.unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let names: Vec<String> =
        archive.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
    assert!(names.contains(&"plugin-1.0.0/package.json".to_string()), "{names:?}");
    let zip = get("acme/plugin/zip/main".into()).await.unwrap();
    assert_eq!(zip.headers()["content-type"], "application/zip");
    assert!(zip.bytes().await.unwrap().starts_with(b"PK"));

    // Upstream moves on; after the max age the mirror refreshes.
    std::fs::write(src.join("README.md"), "v2\n").unwrap();
    git(&src, &["commit", "-q", "-am", "two"]);
    git(&src, &["push", "-q", bare.to_str().unwrap(), "main"]);
    s.publish_dumb_git("forge", &bare, "acme/plugin").await;
    let head = git(&src, &["rev-parse", "HEAD"]).trim().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    git(&clone, &["pull", "-q", "--ff-only"]);
    assert_eq!(std::fs::read_to_string(clone.join("README.md")).unwrap(), "v2\n");
    // Pinned commits are immutable and cacheable.
    let r = get(format!("acme/plugin/raw/{head}/README.md")).await.unwrap();
    assert!(r.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert_eq!(r.text().await.unwrap(), "v2\n");

    // Pushes are refused; unknown repositories are 404.
    let push = s.client.get(format!("{base}/acme/plugin.git/info/refs?service=git-receive-pack")).send().await.unwrap();
    assert_eq!(push.status(), StatusCode::FORBIDDEN);
    let missing = s.client.get(format!("{base}/acme/nope.git/info/refs?service=git-upload-pack")).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let bad = s.client.get(format!("{base}/acme/plugin/archive/--output=x.tar.gz")).send().await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // The remote disappears: the mirror keeps serving the last state.
    let r = s.admin(reqwest::Method::DELETE, "/repository/forge/acme/").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let again = work.path().join("again");
    git(work.path(), &["clone", "-q", &format!("{base}/acme/plugin.git"), again.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(again.join("README.md")).unwrap(), "v2\n");

    // Admins can evict a mirror; deleting the repository removes all mirrors.
    let anon_delete = s.client.delete(format!("{base}/acme/plugin")).send().await.unwrap();
    assert_eq!(anon_delete.status(), StatusCode::UNAUTHORIZED);
    let r = s.admin(reqwest::Method::DELETE, "/repository/github/acme/plugin").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let gone = s.client.get(format!("{base}/acme/plugin.git/info/refs?service=git-upload-pack")).send().await.unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    let r = s.admin(reqwest::Method::DELETE, "/api/v1/repositories/github").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn skills_marketplace_proxy_caches_downloads_and_rewrites_urls() {
    // A tiny ClawHub-style upstream.
    use axum::extract::{Query, State as AxState};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone)]
    struct Up {
        base: Arc<std::sync::OnceLock<String>>,
        hits: Arc<AtomicUsize>,
        down: Arc<std::sync::atomic::AtomicBool>,
    }
    let up = Up { base: Default::default(), hits: Default::default(), down: Default::default() };
    async fn search(AxState(u): AxState<Up>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
        use axum::response::IntoResponse;
        if u.down.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        u.hits.fetch_add(1, Ordering::SeqCst);
        let base = u.base.get().unwrap();
        axum::Json(json!({"results": [{"slug": "pdf-parser", "query": q.get("q"),
            "downloadUrl": format!("{base}/api/v1/download?slug=pdf-parser&version=1.0.0")}]}))
        .into_response()
    }
    async fn download(AxState(u): AxState<Up>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
        use axum::response::IntoResponse;
        if u.down.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        u.hits.fetch_add(1, Ordering::SeqCst);
        let body = format!("PK-zip-of-{}-{}", q["slug"], q.get("version").map(String::as_str).unwrap_or("latest"));
        ([("content-type", "application/zip")], body).into_response()
    }
    let app = axum::Router::new()
        .route("/api/v1/search", axum::routing::get(search))
        .route("/api/v1/download", axum::routing::get(download))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    up.base.set(upstream.clone()).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let s = Server::start().await;
    s.create_repo(json!({"name": "skillhub", "format": "skills", "kind": "proxy",
        "config": {"proxy": {"remote_url": upstream, "metadata_max_age_secs": 1, "outbound": {"mode": "direct"}}}}))
        .await;
    let base = s.url("/repository/skillhub");

    let doc: Value =
        s.client.get(format!("{base}/api/v1/search?q=pdf&limit=5")).send().await.unwrap().json().await.unwrap();
    assert_eq!(doc["results"][0]["query"], "pdf");
    assert_eq!(doc["results"][0]["downloadUrl"], format!("{base}/api/v1/download?slug=pdf-parser&version=1.0.0"));
    // Same query in another order → cache hit.
    let before = up.hits.load(Ordering::SeqCst);
    let r = s.client.get(format!("{base}/api/v1/search?limit=5&q=pdf")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(up.hits.load(Ordering::SeqCst), before);

    let zip = s.client.get(format!("{base}/api/v1/download?slug=pdf-parser&version=1.0.0")).send().await.unwrap();
    assert_eq!(zip.headers()["content-type"], "application/zip");
    assert_eq!(zip.text().await.unwrap(), "PK-zip-of-pdf-parser-1.0.0");
    let latest = s.client.get(format!("{base}/api/v1/download?slug=pdf-parser")).send().await.unwrap();
    assert_eq!(latest.text().await.unwrap(), "PK-zip-of-pdf-parser-latest");

    // The pinned download is browsable under a readable path.
    let tree: Value = s
        .client
        .get(s.url("/api/v1/repositories/skillhub/browse?path=api/v1/download/pdf-parser/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tree.to_string().contains("1.0.0"), "{tree}");

    // Upstream outage: cached search results and downloads keep working.
    up.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let r = s.client.get(format!("{base}/api/v1/search?q=pdf&limit=5")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let zip = s.client.get(format!("{base}/api/v1/download?version=1.0.0&slug=pdf-parser")).send().await.unwrap();
    assert_eq!(zip.text().await.unwrap(), "PK-zip-of-pdf-parser-1.0.0");
    let r = s.client.get(format!("{base}/api/v1/search?q=other")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);

    // Discovery document points clients at Trove; the proxy is read-only.
    let wk: Value =
        s.client.get(format!("{base}/.well-known/clawhub.json")).send().await.unwrap().json().await.unwrap();
    assert_eq!(wk["apiBase"], base);
    let post = s.client.post(format!("{base}/api/v1/skills")).body("x").send().await.unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn raw_proxy_content_max_age_revalidates() {
    let s = Server::start().await;
    s.create_repo(
        json!({"name": "origin", "format": "raw", "kind": "hosted", "config": {"write_policy": "allow_redeploy"}}),
    )
    .await;
    s.admin(reqwest::Method::PUT, "/repository/origin/index.json").body("v1").send().await.unwrap();
    s.create_repo(json!({"name": "immutable", "format": "raw", "kind": "proxy",
        "config": {"proxy": {"remote_url": s.url("/repository/origin/"), "outbound": {"mode": "direct"}}}}))
        .await;
    s.create_repo(json!({"name": "ttl", "format": "raw", "kind": "proxy",
        "config": {"proxy": {"remote_url": s.url("/repository/origin/"), "content_max_age_secs": 1, "outbound": {"mode": "direct"}}}}))
    .await;
    let get = |r: &str| s.client.get(s.url(&format!("/repository/{r}/index.json"))).send();
    assert_eq!(get("immutable").await.unwrap().text().await.unwrap(), "v1");
    assert_eq!(get("ttl").await.unwrap().text().await.unwrap(), "v1");
    s.admin(reqwest::Method::PUT, "/repository/origin/index.json").body("v2").send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(get("immutable").await.unwrap().text().await.unwrap(), "v1");
    assert_eq!(get("ttl").await.unwrap().text().await.unwrap(), "v2");
    let cfg: Value =
        s.admin(reqwest::Method::GET, "/api/v1/repositories/ttl").send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["config"]["proxy"]["content_max_age_secs"], 1);
}
