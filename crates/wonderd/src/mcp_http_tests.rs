//! HTTP contracts for discovery, owner consent, credentials and tool authority.
//! The provider is the existing Python App Server fixture: no live models.
use crate::*;
use axum::body::to_bytes;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:43123";
const ISSUER: &str = "http://127.0.0.1:43123";
const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUV0123456789-._~";
struct Fx {
    dir: tempfile::TempDir,
    app: AppState,
    router: Router,
}
struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}
impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap()
    }
    fn html(&self) -> String {
        String::from_utf8(self.bytes.clone()).unwrap()
    }
}
impl Fx {
    async fn new() -> Self {
        let (dir, mut app) = crate::test_support::fixture().await;
        app.public_origin = "https://wonder.fixture.ts.net".into();
        app.public_origin_file = None;
        let router = crate::router(app.clone());
        Self { dir, app, router }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", HOST);
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            bytes,
        }
    }
    async fn json(&self, method: &str, path: &str, body: Value, headers: &[(&str, &str)]) -> Reply {
        let mut h = vec![("content-type", "application/json")];
        h.extend_from_slice(headers);
        self.request(method, path, body.to_string(), &h).await
    }
    async fn form(&self, path: &str, fields: &[(&str, &str)], headers: &[(&str, &str)]) -> Reply {
        let mut h = vec![("content-type", "application/x-www-form-urlencoded")];
        h.extend_from_slice(headers);
        self.request("POST", path, encode(fields), &h).await
    }
    async fn client(&self) -> String {
        let reply=self.json("POST","/oauth/mcp/register",json!({"client_name":"Fixture CLI","redirect_uris":["http://localhost:14000/callback"],"token_endpoint_auth_method":"client_secret_post","grant_types":["authorization_code","refresh_token"]}),&[]).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let json = reply.json();
        assert_eq!(json["token_endpoint_auth_method"], "none");
        assert!(json.get("client_secret").is_none());
        json["client_id"].as_str().unwrap().into()
    }
    async fn begin(&self, client: &str, scopes: &str) -> (String, String, String) {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
        let path = format!(
            "/oauth/mcp/authorize?{}",
            encode(&[
                ("client_id", client),
                ("redirect_uri", "http://localhost:14999/callback"),
                ("response_type", "code"),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
                ("state", "owner-state"),
                ("scope", scopes),
                ("resource", &format!("{ISSUER}/mcp"))
            ])
        );
        let reply = self.request("GET", &path, String::new(), &[]).await;
        assert_eq!(reply.status, StatusCode::OK);
        let html = reply.html();
        assert!(html.contains("Fixture CLI"));
        assert!(html.contains("localhost:14999/callback"));
        assert!(html.contains("Read only is the default"));
        assert!(!html.contains("name=\"allow_start\" value=\"yes\" checked"));
        let id = hidden(&html, "request_id");
        let csrf = hidden(&html, "csrf");
        let cookie = reply.headers["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (id, csrf, cookie)
    }
    async fn pairing(&self) -> String {
        let r = self
            .json(
                "POST",
                "/api/v1/mcp/pairing-code",
                json!({}),
                &[(
                    "x-wonder-loopback-capability",
                    &self.app.loopback_capability,
                )],
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        r.json()["pairingCode"].as_str().unwrap().into()
    }
    async fn approve(&self, client: &str, requested: &str, approved: &str, mode: &str) -> String {
        let (id, csrf, cookie) = self.begin(client, requested).await;
        let pairing = self.pairing().await;
        let mut fields = vec![
            ("request_id", id.as_str()),
            ("csrf", csrf.as_str()),
            ("pairing_code", pairing.as_str()),
            ("permission_mode", mode),
            ("decision", "approve"),
        ];
        if approved.split_whitespace().any(|s| s == "start") {
            fields.push(("allow_start", "yes"));
        }
        if approved.split_whitespace().any(|s| s == "message") {
            fields.push(("allow_message", "yes"));
        }
        if approved.split_whitespace().any(|s| s == "stop") {
            fields.push(("allow_stop", "yes"));
        }
        if approved.split_whitespace().any(|s| s == "simulator") {
            fields.push(("allow_simulator", "yes"));
        }
        let r = self
            .form(
                "/oauth/mcp/authorize",
                &fields,
                &[("cookie", &cookie), ("origin", ISSUER)],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        let location = Url::parse(r.headers["location"].to_str().unwrap()).unwrap();
        assert!(location
            .query_pairs()
            .any(|(k, v)| k == "state" && v == "owner-state"));
        location
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned()
    }
    async fn exchange(&self, client: &str, code: &str, verifier: &str) -> Reply {
        self.form(
            "/oauth/mcp/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", client),
                ("code", code),
                ("redirect_uri", "http://localhost:14999/callback"),
                ("code_verifier", verifier),
                ("resource", &format!("{ISSUER}/mcp")),
            ],
            &[],
        )
        .await
    }
    async fn login(&self, scopes: &str, mode: &str) -> (String, Value) {
        let client = self.client().await;
        let code = self.approve(&client, scopes, scopes, mode).await;
        let reply = self.exchange(&client, &code, VERIFIER).await;
        assert_eq!(reply.status, StatusCode::OK);
        (client, reply.json())
    }
    async fn rpc(&self, token: &str, method: &str, params: Value) -> Reply {
        self.json(
            "POST",
            "/mcp",
            json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
            &[
                ("authorization", &format!("Bearer {token}")),
                ("accept", "application/json, text/event-stream"),
            ],
        )
        .await
    }
    async fn tool(&self, token: &str, name: &str, args: Value) -> Reply {
        self.rpc(token, "tools/call", json!({"name":name,"arguments":args}))
            .await
    }
    async fn db(&self) -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self.dir.path().join("state.db").display()
        ))
        .await
        .unwrap()
    }
}
fn encode(pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    url.query().unwrap_or("").into()
}
fn hidden(html: &str, name: &str) -> String {
    html.split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .into()
}
fn access(tokens: &Value) -> &str {
    tokens["access_token"].as_str().unwrap()
}
fn safe_cli_diagnostic(value: &str) -> String {
    value
        .lines()
        .filter(|line| {
            !line.contains("://")
                && !line.contains("code=")
                && !line.contains("state=")
                && !line.to_ascii_lowercase().contains("token")
                && !line.to_ascii_lowercase().contains("secret")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn mcp_discovery_and_default_read_only_consent() {
    let fx = Fx::new().await;
    let missing = fx
        .json(
            "POST",
            "/mcp",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            &[],
        )
        .await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
    assert!(missing.headers["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("/.well-known/oauth-protected-resource/mcp"));
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let r = fx.request("GET", path, String::new(), &[]).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.json()["resource"], format!("{ISSUER}/mcp"));
    }
    let metadata = fx
        .request(
            "GET",
            "/.well-known/oauth-authorization-server",
            String::new(),
            &[],
        )
        .await
        .json();
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        metadata["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    let client = fx.client().await;
    // Clients can request every advertised scope, but the owner grants only read by default.
    let code = fx
        .approve(&client, "read start message stop", "read", "read_only")
        .await;
    let tokens = fx.exchange(&client, &code, VERIFIER).await;
    assert_eq!(tokens.status, StatusCode::OK);
    assert_eq!(tokens.headers["cache-control"], "no-store");
    assert_eq!(tokens.json()["scope"], "read");
    assert_eq!(tokens.json()["expires_in"], 600);
    let tokens = tokens.json();
    let init=fx.rpc(access(&tokens),"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"Fixture","version":"1"}})).await;
    assert_eq!(init.json()["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init.json()["result"]["serverInfo"]["name"], "Cupola");
    let tools = fx
        .rpc(access(&tokens), "tools/list", json!({}))
        .await
        .json();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);
    let read = fx
        .tool(access(&tokens), "wonder_project_list", json!({}))
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.json()["result"]["isError"], false);
    let project = read.json()["result"]["structuredContent"]["projects"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let listed = fx
        .tool(
            access(&tokens),
            thread_tools::LIST,
            json!({"projectId":project}),
        )
        .await
        .json();
    assert_eq!(listed["result"]["isError"], false);
    let thread = listed["result"]["structuredContent"]["threads"][0]["id"]
        .as_str()
        .unwrap();
    for (name, args) in [
        (thread_tools::READ, json!({"threadId":thread})),
        (
            thread_tools::WAIT,
            json!({"threadId":thread,"timeoutSeconds":1}),
        ),
        ("wonder_provider_list", json!({})),
        ("wonder_model_list", json!({})),
    ] {
        let reply = fx.tool(access(&tokens), name, args).await.json();
        assert_eq!(
            reply["result"]["isError"], false,
            "read scope could not call {name}"
        );
    }
    for tool in [
        "wonder_thread_start",
        thread_tools::SEND,
        "wonder_thread_stop",
    ] {
        assert_eq!(
            fx.tool(access(&tokens), tool, json!({})).await.status,
            StatusCode::FORBIDDEN
        );
    }
    let api = fx
        .json(
            "GET",
            "/api/v1/devices",
            json!({}),
            &[("authorization", &format!("Bearer {}", access(&tokens)))],
        )
        .await;
    assert_eq!(api.status, StatusCode::FORBIDDEN);
    let stream = fx
        .request(
            "GET",
            "/mcp",
            String::new(),
            &[("authorization", &format!("Bearer {}", access(&tokens)))],
        )
        .await;
    assert_eq!(stream.status, StatusCode::METHOD_NOT_ALLOWED);
    let notification = fx
        .json(
            "POST",
            "/mcp",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            &[("authorization", &format!("Bearer {}", access(&tokens)))],
        )
        .await;
    assert_eq!(notification.status, StatusCode::ACCEPTED);
    assert!(notification.bytes.is_empty());
}

#[tokio::test]
async fn mcp_invalid_redirects_pkce_origin_and_local_control_are_refused() {
    let fx = Fx::new().await;
    for uri in [
        "http://example.com/callback",
        "https://user:pw@example.com/callback",
        "https://example.com/callback#",
        "file:///tmp/callback",
        "https://example.com/callback?code=trick",
    ] {
        let r = fx
            .json(
                "POST",
                "/oauth/mcp/register",
                json!({"redirect_uris":[uri]}),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }
    assert!(!mcp_oauth::redirect_matches(
        "http://localhost/a",
        "http://127.0.0.1/a"
    ));
    assert!(!mcp_oauth::redirect_matches(
        "https://service.test/a",
        "https://service.test:9000/a"
    ));
    assert!(!mcp_oauth::redirect_matches(
        "http://localhost/a",
        "http://localhost/b"
    ));
    let hostile = fx
        .json(
            "POST",
            "/oauth/mcp/register",
            json!({"redirect_uris":["http://localhost/a"]}),
            &[("origin", "https://attacker.test")],
        )
        .await;
    assert_eq!(hostile.status, StatusCode::FORBIDDEN);
    let local = fx
        .json("POST", "/api/v1/mcp/pairing-code", json!({}), &[])
        .await;
    assert_eq!(local.status, StatusCode::FORBIDDEN);
    let client = fx.client().await;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    for (redirect, method, resource) in [
        ("http://localhost/other", "S256", format!("{ISSUER}/mcp")),
        (
            "http://localhost/callback",
            "plain",
            format!("{ISSUER}/mcp"),
        ),
        (
            "http://localhost/callback",
            "S256",
            "https://other/mcp".into(),
        ),
    ] {
        let path = format!(
            "/oauth/mcp/authorize?{}",
            encode(&[
                ("client_id", &client),
                ("redirect_uri", redirect),
                ("response_type", "code"),
                ("code_challenge_method", method),
                ("code_challenge", &challenge),
                ("resource", &resource)
            ])
        );
        assert_eq!(
            fx.request("GET", &path, String::new(), &[]).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    // A public hostname cannot become an OAuth issuer via Host or forwarded headers.
    let bad = fx
        .request(
            "GET",
            "/.well-known/oauth-authorization-server",
            String::new(),
            &[("x-forwarded-host", "attacker.test")],
        )
        .await;
    assert_eq!(bad.json()["issuer"], ISSUER);
    let request = Request::builder()
        .uri("/.well-known/oauth-authorization-server")
        .header("host", "attacker.test")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        fx.router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .uri("/api/v1/mcp/clients")
        .header("host", "wonder.fixture.ts.net")
        .header("x-wonder-loopback-capability", &fx.app.loopback_capability)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        fx.router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn mcp_consent_needs_pairing_and_csrf_and_cannot_add_scopes() {
    let fx = Fx::new().await;
    let client = fx.client().await;
    let (id, csrf, cookie) = fx.begin(&client, "read").await;
    let pairing = fx.pairing().await;
    let fields = [
        ("request_id", id.as_str()),
        ("csrf", csrf.as_str()),
        ("pairing_code", pairing.as_str()),
        ("decision", "approve"),
    ];
    assert_eq!(
        fx.form("/oauth/mcp/authorize", &fields, &[]).await.status,
        StatusCode::FORBIDDEN
    );
    let mut escalation = fields.to_vec();
    escalation.push(("allow_start", "yes"));
    assert_eq!(
        fx.form("/oauth/mcp/authorize", &escalation, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let mut wrong = fields.to_vec();
    wrong[2] = ("pairing_code", "WRONG");
    assert_eq!(
        fx.form("/oauth/mcp/authorize", &wrong, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fx.form("/oauth/mcp/authorize", &fields, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        fx.form("/oauth/mcp/authorize", &fields, &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // The pairing credential cannot approve a second authorization request.
    let (id, csrf, cookie) = fx.begin(&client, "read").await;
    assert_eq!(
        fx.form(
            "/oauth/mcp/authorize",
            &[
                ("request_id", &id),
                ("csrf", &csrf),
                ("pairing_code", &pairing),
                ("decision", "approve")
            ],
            &[("cookie", &cookie)]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn mcp_code_replay_wrong_verifier_wrong_client_and_expiry_are_refused() {
    let fx = Fx::new().await;
    let client = fx.client().await;
    let code = fx.approve(&client, "read", "read", "read_only").await;
    assert_eq!(
        fx.exchange(&client, &code, "wrong").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fx.exchange(&client, &code, VERIFIER).await.status,
        StatusCode::BAD_REQUEST
    );
    let code = fx.approve(&client, "read", "read", "read_only").await;
    assert_eq!(
        fx.exchange("different-client", &code, VERIFIER)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let code = fx.approve(&client, "read", "read", "read_only").await;
    assert_eq!(
        fx.exchange(&client, &code, VERIFIER).await.status,
        StatusCode::OK
    );
    assert_eq!(
        fx.exchange(&client, &code, VERIFIER).await.status,
        StatusCode::BAD_REQUEST
    );
    let code = fx.approve(&client, "read", "read", "read_only").await;
    let db = fx.db().await;
    sqlx::query("UPDATE mcp_authorization_codes SET expires_at_ms=0")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        fx.exchange(&client, &code, VERIFIER).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn mcp_access_expiry_revocation_wrong_audience_and_hashed_storage() {
    let fx = Fx::new().await;
    let (client, tokens) = fx.login("read", "read_only").await;
    let db = fx.db().await;
    let hashes: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM mcp_tokens")
        .fetch_all(&db)
        .await
        .unwrap();
    assert!(hashes.contains(&mcp_oauth::hash(access(&tokens))));
    assert!(hashes.iter().all(|h| h.len() == 64
        && h != access(&tokens)
        && Some(h.as_str()) != tokens["refresh_token"].as_str()));
    let metadata = fx
        .json(
            "GET",
            "/api/v1/mcp/clients",
            json!({}),
            &[("x-wonder-loopback-capability", &fx.app.loopback_capability)],
        )
        .await;
    assert_eq!(metadata.status, StatusCode::OK);
    assert_eq!(metadata.json()["grants"][0]["permission_mode"], "read_only");
    assert!(!metadata.html().contains(access(&tokens)));
    assert!(!metadata
        .html()
        .contains(tokens["refresh_token"].as_str().unwrap()));
    assert_eq!(
        fx.json(
            "POST",
            "/api/v1/mcp/pairing-code",
            json!({}),
            &[("authorization", &format!("Bearer {}", access(&tokens)))]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // The same daemon serves local and tailnet origins; credentials are audience-bound.
    let r = fx
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "wonder.fixture.ts.net")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", access(&tokens)))
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE mcp_tokens SET expires_at_ms=0 WHERE kind='access'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        fx.rpc(access(&tokens), "ping", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let (other, other_tokens) = fx.login("read", "read_only").await;
    let revoked = fx
        .json(
            "DELETE",
            &format!("/api/v1/mcp/clients/{other}"),
            json!({}),
            &[("x-wonder-loopback-capability", &fx.app.loopback_capability)],
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(
        fx.rpc(access(&other_tokens), "ping", json!({}))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_ne!(client, other);
}

#[tokio::test]
async fn mcp_workspace_reset_refuses_pre_reset_access_refresh_and_authorization_code() {
    let fx = Fx::new().await;
    let (client, tokens) = fx.login("read", "read_only").await;
    let pending_code = fx.approve(&client, "read", "read", "read_only").await;
    assert_eq!(
        fx.rpc(access(&tokens), "ping", json!({})).await.status,
        StatusCode::OK
    );

    fx.app
        .store
        .reset_workspace("owner", "reset")
        .await
        .unwrap();
    assert_eq!(
        fx.rpc(access(&tokens), "ping", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let refresh = fx
        .form(
            "/oauth/mcp/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                ("resource", &format!("{ISSUER}/mcp")),
            ],
            &[],
        )
        .await;
    assert_eq!(refresh.status, StatusCode::BAD_REQUEST);
    assert_eq!(refresh.json()["error"], "invalid_grant");
    let exchange = fx.exchange(&client, &pending_code, VERIFIER).await;
    assert_eq!(exchange.status, StatusCode::BAD_REQUEST);
    assert_eq!(exchange.json()["error"], "invalid_grant");

    // Reset removes registrations; a fresh registration and owner consent work.
    let (_, fresh) = fx.login("read", "read_only").await;
    assert_eq!(
        fx.rpc(access(&fresh), "ping", json!({})).await.status,
        StatusCode::OK
    );
    assert_eq!(
        fx.rpc(access(&tokens), "ping", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn mcp_refresh_rotation_replay_and_scope_escalation_are_refused() {
    let fx = Fx::new().await;
    let (client, tokens) = fx.login("read message", "workspace").await;
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let base = [
        ("grant_type", "refresh_token"),
        ("client_id", client.as_str()),
        ("refresh_token", refresh),
        ("resource", &format!("{ISSUER}/mcp")),
    ];
    let mut escalation = base.to_vec();
    escalation.push(("scope", "read start"));
    assert_eq!(
        fx.form("/oauth/mcp/token", &escalation, &[]).await.status,
        StatusCode::BAD_REQUEST
    );
    let mut narrowed = base.to_vec();
    narrowed.push(("scope", "read"));
    let rotated = fx.form("/oauth/mcp/token", &narrowed, &[]).await;
    assert_eq!(rotated.status, StatusCode::OK);
    assert_eq!(rotated.json()["scope"], "read");
    let rotated = rotated.json();
    assert_eq!(
        fx.tool(access(&tokens), thread_tools::SEND, json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fx.form("/oauth/mcp/token", &base, &[]).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fx.rpc(access(&rotated), "ping", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let next = rotated["refresh_token"].as_str().unwrap();
    assert_eq!(
        fx.form(
            "/oauth/mcp/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("refresh_token", next)
            ],
            &[]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn mcp_start_and_message_cannot_exceed_permission_ceiling() {
    let fx = Fx::new().await;
    let (_, tokens) = fx.login("read start message stop", "read_only").await;
    // Create an included Project using the existing fixture store helper.
    let t = crate::test_support::project_thread(
        &fx.app.store,
        "mcp-existing",
        "Existing",
        fx.dir.path().to_str().unwrap(),
        "fixture",
        Some("fake"),
        None,
        "now",
    )
    .await
    .unwrap();
    let start=fx.tool(access(&tokens),"wonder_thread_start",json!({"projectId":t.project_id,"provider":"codex","model":"fake","prompt":"fixture only","accessMode":"full_access","clientRequestId":uuid::Uuid::new_v4().to_string()})).await;
    assert_eq!(start.status, StatusCode::OK);
    assert_eq!(start.json()["result"]["isError"], true);
    assert!(start.json()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("permission_escalation"));
    fx.app
        .store
        .update_project_conversation(
            &t.conversation_id,
            wonder_store::ProjectConversationPatch {
                access_mode: Some("workspace"),
                ..Default::default()
            },
            "now",
        )
        .await
        .unwrap();
    let send=fx.tool(access(&tokens),thread_tools::SEND,json!({"threadId":t.conversation_id,"message":"fixture only","clientRequestId":uuid::Uuid::new_v4().to_string()})).await;
    assert!(send.json()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("permission_escalation"));
    assert!(fx
        .app
        .store
        .latest_message(&t.conversation_id)
        .await
        .unwrap()
        .is_none());
}

// Explicit opt-in: invokes installed CLIs with temporary per-client config.
// Never runs a model, opens a browser, or changes the owner's MCP configuration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires installed Claude Code and Codex; run explicitly for interoperability"]
async fn mcp_cli_oauth_login_against_loopback_fixture() {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fx = Fx::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let trail = Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests = trail.clone();
    let discovery_ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let ready = discovery_ready.clone();
    let router = fx.router.clone().layer(middleware::from_fn(
        move |request: Request<Body>, next: Next| {
            let requests = requests.clone();
            let ready = ready.clone();
            async move {
                let path = request.uri().path().to_owned();
                let response = if ready.load(std::sync::atomic::Ordering::SeqCst) {
                    next.run(request).await
                } else {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                };
                requests
                    .lock()
                    .unwrap()
                    .push((path, response.status().as_u16()));
                response
            }
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for (index, cli) in ["claude", "codex"].into_iter().enumerate() {
        let config = fx.dir.path().join(format!("{cli}-config"));
        std::fs::create_dir_all(&config).unwrap();
        if cli == "codex" {
            std::fs::write(
                config.join("config.toml"),
                "mcp_oauth_credentials_store = \"file\"\n",
            )
            .unwrap();
        }
        let name = format!("wonder-fixture-{}", uuid::Uuid::new_v4());
        let url = format!("{origin}/mcp");
        let mut add = tokio::process::Command::new(cli);
        add.env("CLAUDE_CONFIG_DIR", &config)
            .env("CODEX_HOME", &config)
            .current_dir(fx.dir.path());
        if cli == "claude" {
            add.args(["mcp", "add", "--transport", "http", &name, &url]);
        } else {
            add.args(["mcp", "add", &name, "--url", &url]);
            // Codex add otherwise opens a browser for immediate OAuth. Model
            // a starting daemon during configuration, then test its explicit
            // --no-browser login against the real authenticated router.
            discovery_ready.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let result = add.output().await.unwrap();
        discovery_ready.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(result.status.success(), "MCP add failed for {cli}");
        // Claude requires an interactive terminal. Codex explicitly supports
        // piped callback lines, avoiding its terminal raw-input path here.
        // The private PTY writes no transcript (/dev/null).
        let mut login = if cli == "claude" {
            let mut command = tokio::process::Command::new("/usr/bin/script");
            command.args([
                "-q",
                "/dev/null",
                cli,
                "mcp",
                "login",
                &name,
                "--no-browser",
            ]);
            command
        } else {
            let mut command = tokio::process::Command::new(cli);
            command.args(["mcp", "login", &name, "--no-browser"]);
            command
        };
        let mut child = login
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("CODEX_HOME", &config)
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .current_dir(fx.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, mut output) = tokio::sync::mpsc::channel::<String>(32);
        async fn capture(
            mut stream: impl tokio::io::AsyncRead + Unpin,
            sender: tokio::sync::mpsc::Sender<String>,
        ) {
            let mut buffer = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                if sender
                    .send(String::from_utf8_lossy(&buffer[..n]).into_owned())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
        let out_task = tokio::spawn(capture(stdout, sender.clone()));
        let err_task = tokio::spawn(capture(stderr, sender));
        // Keep all CLI output in memory: URLs can carry OAuth state and codes.
        let (auth_url, mut captured) =
            tokio::time::timeout(std::time::Duration::from_secs(40), async {
                let mut captured = String::new();
                while let Some(chunk) = output.recv().await {
                    captured.push_str(&chunk);
                    if let Some(start) = captured.find(&format!("{origin}/oauth/mcp/authorize?")) {
                        // A terminal hyperlink includes a hidden URL and its
                        // visible copy, separated by ESC/BEL rather than space.
                        // Wait for the complete first URL and stop at controls.
                        let Some(end) = captured[start..].find(|ch:char| ch.is_whitespace() || ch.is_control()) else { continue; };
                        let raw = captured[start..start+end].trim_end_matches(['\'', '"']);
                        if let Ok(url) = Url::parse(raw) {
                            if url.query_pairs().any(|(k, _)| k == "code_challenge")
                                && url.query_pairs().any(|(k, _)| k == "state")
                            {
                                return (url, captured);
                            }
                        }
                    }
                }
                panic!("{cli} exited without an authorization URL; safe output: {}; HTTP path/status only: {:?}", safe_cli_diagnostic(&captured), trail.lock().unwrap());
            })
            .await
            .expect("CLI did not begin OAuth within 40 seconds");
        let shape: Vec<_> = auth_url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.len()))
            .collect();
        let page = http.get(auth_url).send().await.unwrap();
        if !page.status().is_success() {
            let status = page.status();
            let body = page.json::<Value>().await.unwrap_or(json!({}));
            panic!("{cli} authorization page refused {status}: {}; parameter names/lengths only: {shape:?}; HTTP path/status only: {:?}", body["error_description"].as_str().unwrap_or("No description"), trail.lock().unwrap());
        }
        let cookie = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let html = page.text().await.unwrap();
        let id = hidden(&html, "request_id");
        let csrf = hidden(&html, "csrf");
        let pairing = http
            .post(format!("{origin}/api/v1/mcp/pairing-code"))
            .header("x-wonder-loopback-capability", &fx.app.loopback_capability)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        let form = encode(&[
            ("request_id", &id),
            ("csrf", &csrf),
            ("pairing_code", pairing["pairingCode"].as_str().unwrap()),
            ("decision", "approve"),
            ("permission_mode", "read_only"),
        ]);
        let approved = http
            .post(format!("{origin}/oauth/mcp/authorize"))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, &origin)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::SEE_OTHER);
        let callback = approved.headers()[header::LOCATION].to_str().unwrap();
        // Both clients' --no-browser flow accepts the callback URL on stdin.
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{callback}\n").as_bytes())
            .await
            .unwrap();
        drop(child.stdin.take());
        let status = tokio::time::timeout(std::time::Duration::from_secs(40), child.wait())
            .await
            .expect("CLI OAuth callback timed out")
            .unwrap();
        // Terminal helpers can retain pipe descriptors after the CLI exits.
        // The exit status and durable grant prove completion; do not wait for
        // unrelated descriptor lifetimes before checking those results.
        out_task.abort();
        err_task.abort();
        let _ = out_task.await;
        let _ = err_task.await;
        while let Ok(chunk) = output.try_recv() {
            captured.push_str(&chunk);
        }
        if !status.success() {
            let safe = safe_cli_diagnostic(&captured);
            eprintln!(
                "{cli} safe diagnostic: {safe}\nHTTP path/status only: {:?}",
                trail.lock().unwrap()
            );
        }
        // A grant in this daemon proves the client's code exchange completed.
        let db = fx.db().await;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mcp_grants WHERE scopes='read' AND permission_mode='read_only'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let logout = tokio::process::Command::new(cli)
            .args(["mcp", "logout", &name])
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("CODEX_HOME", &config)
            .current_dir(fx.dir.path())
            .output()
            .await
            .unwrap();
        assert!(
            logout.status.success(),
            "Fixture credential cleanup failed for {cli}"
        );
        assert!(status.success(), "MCP login failed for {cli}");
        assert_eq!(
            count,
            index as i64 + 1,
            "CLI did not exchange an authorization code"
        );
        eprintln!(
            "{cli}: isolated mcp add + login + owner pairing + token exchange + logout passed"
        );
    }
    server.abort();
}

#[tokio::test]
async fn mcp_start_uses_granted_permissions_and_durable_labelled_idempotent_message() {
    let fx = Fx::new().await;
    std::fs::write(
        fx.dir.path().join("models.json"),
        json!({"data":[{"id":"fake","displayName":"Fake"}],"nextCursor":null}).to_string(),
    )
    .unwrap();
    let _service = crate::ingestion::spawn(fx.app.clone()).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !fx
        .app
        .ingestion
        .project_readiness(&fx.app.store)
        .await
        .ready
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "synthetic provider failed to become ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, tokens) = fx.login("read start", "read_only").await;
    let project = fx
        .app
        .store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.is_included)
        .unwrap();
    let args = json!({"projectId":project.id,"provider":"codex","model":"fake","prompt":"Read the synthetic fixture only.","clientRequestId":uuid::Uuid::new_v4().to_string()});
    let started = fx
        .tool(access(&tokens), "wonder_thread_start", args.clone())
        .await
        .json();
    assert_eq!(
        started["result"]["isError"], false,
        "synthetic start failed"
    );
    let thread = started["result"]["structuredContent"]["threadId"]
        .as_str()
        .unwrap();
    let stored = fx
        .app
        .store
        .project_conversation(thread)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.access_mode, "read_only");
    assert_eq!(stored.claude_approval, "ask");
    let message = fx.app.store.latest_message(thread).await.unwrap().unwrap();
    let source = fx
        .app
        .store
        .message_source(&message.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.source_title, "Fixture CLI");
    assert_eq!(source.source_conversation_id, None);
    let retried = fx
        .tool(access(&tokens), "wonder_thread_start", args)
        .await
        .json();
    assert_eq!(retried["result"]["structuredContent"]["threadId"], thread);
    assert_eq!(
        fx.app.store.open_message_ids(thread).await.unwrap(),
        vec![message.id]
    );
    // Start permission covers only the initial message; it does not grant send.
    assert_eq!(
        fx.tool(access(&tokens), thread_tools::SEND, json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

/// Simulator tools need their own consent; with it, agents get the phone's
/// typed actions and a PNG as MCP image content, never touch input, and
/// Cupola's test simulators stay refused under a wonderctl lease.
#[tokio::test]
async fn mcp_simulator_tools_need_their_scope_and_return_typed_results() {
    use crate::simulators::tests::{png, Fake, Rig, BOOTED, TEST_DEVICE};
    let mut fx = Fx::new().await;
    let rig = Rig::with(Fake {
        screenshot: std::sync::Mutex::new(Some(png(2, 3))),
        ..Fake::default()
    });
    rig.hold_lease(chrono::Local::now().naive_local() + chrono::Duration::minutes(5));
    fx.app.simulators = Arc::new(rig.shared());
    fx.router = crate::router(fx.app.clone());

    // Requested but not ticked: the owner did not grant it.
    let client = fx.client().await;
    let code = fx
        .approve(&client, "read simulator", "read", "read_only")
        .await;
    let reader = fx.exchange(&client, &code, VERIFIER).await.json();
    assert_eq!(reader["scope"], "read");
    let tools = fx
        .rpc(access(&reader), "tools/list", json!({}))
        .await
        .json();
    assert!(!tools.to_string().contains("wonder_simulator_"));
    let refused = fx
        .tool(access(&reader), "wonder_simulator_list", json!({}))
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.headers["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("scope=\"simulator\""));

    let (_, tokens) = fx.login("simulator", "read_only").await;
    assert_eq!(tokens["scope"], "simulator");
    let token = access(&tokens);
    let tools = fx.rpc(token, "tools/list", json!({})).await.json();
    let names: Vec<(String, bool)> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_owned(),
                t["annotations"]["readOnlyHint"].as_bool().unwrap(),
            )
        })
        .collect();
    let expected = [
        ("wonder_simulator_list", true),
        ("wonder_simulator_settings", true),
        ("wonder_simulator_screenshot", true),
        ("wonder_simulator_apps", true),
        ("wonder_simulator_set_appearance", false),
        ("wonder_simulator_set_content_size", false),
        ("wonder_simulator_set_language", false),
        ("wonder_simulator_reset_permissions", false),
        ("wonder_simulator_open_url", false),
    ]
    .map(|(name, read_only)| (name.to_owned(), read_only));
    assert_eq!(names, expected);

    let list = fx
        .tool(token, "wonder_simulator_list", json!({}))
        .await
        .json();
    let list = &list["result"]["structuredContent"];
    assert_eq!(list["host"]["id"], fx.app.host_installation_id);
    assert_eq!(list["agentInput"]["available"], false);
    assert_eq!(list["simulators"][0]["udid"], BOOTED);

    let set = fx
        .tool(
            token,
            "wonder_simulator_set_appearance",
            json!({"udid":BOOTED,"appearance":"dark"}),
        )
        .await
        .json();
    assert_eq!(
        set["result"]["structuredContent"],
        json!({"udid":BOOTED,"appearance":"dark","applied":true})
    );
    let shot = fx
        .tool(token, "wonder_simulator_screenshot", json!({"udid":BOOTED}))
        .await
        .json();
    let content = &shot["result"]["content"];
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["mimeType"], "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content[0]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, png(2, 3));
    assert_eq!(shot["result"]["structuredContent"]["width"], 2);
    assert!(shot["result"]["structuredContent"]
        .get("imageBase64")
        .is_none());

    // Policy and schema refusals; no simulator command runs for either.
    let before = rig.actions().len();
    let bad = fx
        .tool(
            token,
            "wonder_simulator_open_url",
            json!({"udid":BOOTED,"url":"file:///etc/hosts"}),
        )
        .await
        .json();
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"invalid\""));
    let schema = fx
        .tool(
            token,
            "wonder_simulator_set_appearance",
            json!({"udid":BOOTED,"appearance":"sepia"}),
        )
        .await
        .json();
    assert_eq!(schema["error"]["code"], -32602);
    let leased = fx
        .tool(
            token,
            "wonder_simulator_settings",
            json!({"udid":TEST_DEVICE}),
        )
        .await
        .json();
    let text = leased["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("simulator-in-use-by-tests") && text.contains("being used for tests"));
    assert_eq!(rig.actions().len(), before);
}

#[tokio::test]
async fn project_list_names_each_projects_repository_for_grouping_only() {
    let fx = Fx::new().await;
    let base = crate::canonical_path::canonicalize(fx.dir.path()).unwrap();
    let checkout = base.join("Cupola");
    let plain = base.join("Notes");
    std::fs::create_dir_all(checkout.join("apps")).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:Swaymun/Wonder.git",
        ],
    ] {
        let status = crate::test_git::git()
            .arg("-C")
            .arg(&checkout)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    for (id, folder) in [("in-repo", checkout.join("apps")), ("no-repo", plain)] {
        let folder = folder.to_str().unwrap();
        fx.app
            .store
            .create_project(
                id,
                id,
                "fixture",
                id,
                &[wonder_store::ProjectRootInput {
                    path: folder.into(),
                    canonical_path: folder.into(),
                }],
                0,
                "2026-10-26T00:00:00Z",
            )
            .await
            .unwrap();
    }
    let (_, tokens) = fx.login("read", "read_only").await;
    let listed = fx
        .tool(access(&tokens), "wonder_project_list", json!({}))
        .await
        .json();
    let projects = listed["result"]["structuredContent"]["projects"]
        .as_array()
        .unwrap();
    let find = |id: &str| projects.iter().find(|p| p["id"] == id).unwrap().clone();
    assert_eq!(
        find("in-repo")["repository"],
        json!({"key":"v1:github.com/swaymun/wonder","subpath":"apps","name":"Wonder"})
    );
    assert!(find("no-repo").get("repository").is_none());
}
