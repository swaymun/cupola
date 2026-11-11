use super::*;
#[cfg(target_os = "macos")]
use axum::{body::to_bytes, routing::get, Router};
use serde_json::Value;
#[cfg(target_os = "macos")]
use tower::ServiceExt;

#[test]
fn titles_are_decoded_trimmed_and_bounded() {
    assert_eq!(
        page_title("<html><head><TITLE>\n  Vite &amp; React\n</TITLE></head>").as_deref(),
        Some("Vite & React")
    );
    assert_eq!(page_title("<title>   </title>"), None);
    assert_eq!(page_title("<h1>No title</h1>"), None);
    assert_eq!(
        page_title(&format!("<title>{}</title>", "a".repeat(200)))
            .unwrap()
            .len(),
        80
    );
}

#[test]
fn project_membership_matches_whole_folders() {
    let roots = [PathBuf::from("/work/app")];
    assert!(in_scope(Path::new("/work/app"), &roots, &[]));
    assert!(in_scope(Path::new("/work/app/web"), &roots, &[]));
    assert!(!in_scope(Path::new("/work/app-old"), &roots, &[]));
    assert!(!in_scope(Path::new("/work"), &roots, &[]));
    // Another thread's worktree inside the main checkout is not this thread's.
    let nested = [PathBuf::from("/work/app/.local/wt-fix")];
    assert!(in_scope(Path::new("/work/app/web"), &roots, &nested));
    assert!(in_scope(
        Path::new("/work/app/.local/wt-fix-2"),
        &roots,
        &nested
    ));
    assert!(!in_scope(
        Path::new("/work/app/.local/wt-fix"),
        &roots,
        &nested
    ));
    assert!(!in_scope(
        Path::new("/work/app/.local/wt-fix/web"),
        &roots,
        &nested
    ));
}

#[test]
fn only_web_pages_count_as_dev_servers() {
    for (status, content_type, location, page) in [
        (200, Some("text/html"), None, true),
        (200, Some("Text/HTML; charset=utf-8"), None, true),
        (200, Some("application/xhtml+xml"), None, true),
        (302, None, Some("/login"), true),
        (307, None, Some("http://localhost:5173/app"), true),
        // Inspectors and debuggers (node --inspect, workerd) and APIs.
        (200, Some("application/json; charset=UTF-8"), None, false),
        (200, Some("text/plain"), None, false),
        (200, None, None, false),
        (400, Some("text/html"), None, false),
        (404, Some("text/html"), None, false),
        (426, Some("text/plain"), None, false),
        (500, Some("text/html"), None, false),
        // A redirect away from the server, or nowhere, is not its page.
        (302, None, Some("https://accounts.example.com/"), false),
        (302, None, Some("http://localhost:9229/"), false),
        (302, None, None, false),
    ] {
        assert_eq!(
            serves_page(status, content_type, location, 5173),
            page,
            "{status} {content_type:?} {location:?}"
        );
    }
}

#[test]
fn only_loopback_reachable_listeners_are_targets() {
    let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    assert_eq!(loopback_target("127.0.0.1".parse().unwrap()), Some(v4));
    assert_eq!(loopback_target("0.0.0.0".parse().unwrap()), Some(v4));
    assert_eq!(loopback_target("::1".parse().unwrap()), Some(v6));
    assert_eq!(loopback_target("::".parse().unwrap()), Some(v6));
    assert_eq!(
        loopback_target("::ffff:127.0.0.1".parse().unwrap()),
        Some(v4)
    );
    assert_eq!(loopback_target("192.168.1.20".parse().unwrap()), None);
    assert_eq!(loopback_target("fe80::1".parse().unwrap()), None);
}

// Contract: a localhost link in an agent's reply opens in the preview when
// its port runs for the thread, even past the picker's limit; the picker
// itself keeps showing the lowest ports.
#[test]
fn a_link_asks_about_its_own_port_only() {
    let found: Vec<Listener> = (0..MAX_SERVERS as u16 + 3)
        .map(|offset| Listener {
            port: 3000 + offset,
            pid: 1,
            process_name: "node".into(),
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        })
        .collect();
    let ports = |port| {
        candidates(&found, port)
            .iter()
            .map(|listener| listener.port)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ports(None),
        (3000..3000 + MAX_SERVERS as u16).collect::<Vec<_>>()
    );
    let beyond = 3000 + MAX_SERVERS as u16 + 2;
    assert_eq!(ports(Some(beyond)), vec![beyond]);
    assert_eq!(ports(Some(3001)), vec![3001]);
    assert!(ports(Some(9229)).is_empty());
}

#[test]
fn proxied_paths_cannot_leave_the_dev_server() {
    let base = "/api/v1/projects/p1/preview/5173";
    assert_eq!(upstream_path(base, None, 5173).as_deref(), Some("/"));
    assert_eq!(
        upstream_path(&format!("{base}/"), None, 5173).as_deref(),
        Some("/")
    );
    assert_eq!(
        upstream_path(&format!("{base}/assets/app.js"), Some("v=1"), 5173).as_deref(),
        Some("/assets/app.js?v=1")
    );
    assert_eq!(
        upstream_path(&format!("{base}/@vite/client"), None, 5173).as_deref(),
        Some("/@vite/client")
    );
    for refused in [
        format!("{base}/../../conversations"),
        format!("{base}/a/%2e%2e/b"),
        format!("{base}/a/%2E%2E/b"),
        format!("{base}/a%2f..%2fb"),
        format!("{base}/a%5c..%5cb"),
        format!("{base}/a\\b"),
        format!("{base}/./a"),
        format!("{base}/%zz"),
        "/api/v1/projects/p1/preview/5174/a".to_owned(),
        "/api/v1/conversations/p1/preview/5173/a".to_owned(),
    ] {
        assert_eq!(upstream_path(&refused, None, 5173), None, "{refused}");
    }
}

#[test]
fn redirects_stay_on_the_dev_server_or_are_named_external() {
    assert_eq!(rewrite_location("/login", "/", 5173), Ok("/login".into()));
    assert_eq!(
        rewrite_location("next?x=1#top", "/docs/intro", 5173),
        Ok("/docs/next?x=1#top".into())
    );
    assert_eq!(
        rewrite_location("http://localhost:5173/home", "/", 5173),
        Ok("/home".into())
    );
    assert_eq!(
        rewrite_location("http://127.0.0.1:5173/home", "/", 5173),
        Ok("/home".into())
    );
    assert_eq!(
        rewrite_location("https://accounts.example.com/sign-in", "/", 5173),
        Err(Some("https://accounts.example.com/sign-in".into()))
    );
    assert_eq!(
        rewrite_location("http://localhost:9000/", "/", 5173),
        Err(Some("http://localhost:9000/".into()))
    );
    assert_eq!(
        rewrite_location("javascript:alert(1)", "/", 5173),
        Err(None)
    );
}

#[test]
fn wonder_credentials_never_reach_the_dev_server() {
    let mut incoming = HeaderMap::new();
    for (name, value) in [
        ("cookie", "__Host-wonder_session=secret"),
        ("x-wonder-csrf", "csrf"),
        ("x-wonder-loopback-capability", "capability"),
        ("authorization", "Bearer token"),
        ("origin", "https://wonder.test"),
        ("referer", "https://wonder.test/"),
        ("host", "wonder.test"),
        ("x-forwarded-for", "10.0.0.2"),
        ("accept", "text/html"),
        ("if-none-match", "\"abc\""),
        (COOKIE_HEADER, "theme=dark"),
    ] {
        incoming.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    let get = forward_request_headers(&incoming, 5173, &reqwest::Method::GET);
    assert_eq!(get.get("cookie").unwrap(), "theme=dark");
    assert_eq!(get.get("host").unwrap(), "localhost:5173");
    assert_eq!(get.get("accept").unwrap(), "text/html");
    assert_eq!(get.get("if-none-match").unwrap(), "\"abc\"");
    assert_eq!(get.get("accept-encoding").unwrap(), "identity");
    for absent in [
        "x-wonder-csrf",
        "x-wonder-loopback-capability",
        "authorization",
        "origin",
        "referer",
        "x-forwarded-for",
        COOKIE_HEADER,
    ] {
        assert!(get.get(absent).is_none(), "{absent}");
    }
    let post = forward_request_headers(&incoming, 5173, &reqwest::Method::POST);
    assert_eq!(post.get("origin").unwrap(), "http://localhost:5173");
}

/// A dev server inside this test process: pages, header echo, redirects,
/// cookies and a hot-reload socket that echoes with its subprotocol.
#[cfg(target_os = "macos")]
async fn dev_server() -> (u16, tokio::task::JoinHandle<()>) {
    async fn echo(headers: HeaderMap) -> Json<Value> {
        Json(Value::Object(
            headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        Value::String(value.to_str().unwrap_or_default().to_owned()),
                    )
                })
                .collect(),
        ))
    }
    async fn socket(upgrade: WebSocketUpgrade) -> Response {
        upgrade
            .protocols(["vite-hmr"])
            .on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Text(text) = message {
                        let reply = format!("echo:{}", text.as_str());
                        if socket.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                }
            })
    }
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/html"),
                        (header::SET_COOKIE, "session=dev; Path=/"),
                        (header::CONTENT_SECURITY_POLICY, "default-src 'self'"),
                    ],
                    "<!doctype html><title>Fixture App</title><script src=\"/assets/app.js\"></script>",
                )
            }),
        )
        .route(
            "/assets/app.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], "console.log(1)") }),
        )
        .route("/echo", get(echo).post(echo))
        .route(
            "/login-redirect",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/login")]) }),
        )
        .route(
            "/away",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "https://accounts.example.com/")],
                )
            }),
        )
        .route("/hmr", get(socket));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, task)
}

/// A Project whose folder holds this test process's working directory, so
/// its in-process listeners count as the Project's dev servers.
#[cfg(target_os = "macos")]
async fn project_here(state: &AppState, id: &str) {
    let here = crate::canonical_path::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let here = here.to_str().unwrap();
    state
        .store
        .create_project(
            id,
            id,
            "fixture",
            "Here",
            &[wonder_store::ProjectRootInput {
                path: here.into(),
                canonical_path: here.into(),
            }],
            0,
            "now",
        )
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
async fn project_elsewhere(state: &AppState, id: &str, dir: &Path) {
    let folder = dir.join(id);
    std::fs::create_dir_all(&folder).unwrap();
    let folder = crate::canonical_path::canonicalize(folder).unwrap();
    let folder = folder.to_str().unwrap();
    state
        .store
        .create_project(
            id,
            id,
            "fixture",
            "Elsewhere",
            &[wonder_store::ProjectRootInput {
                path: folder.into(),
                canonical_path: folder.into(),
            }],
            0,
            "now",
        )
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
async fn send(state: &AppState, request: axum::http::Request<Body>) -> Response {
    crate::router(state.clone()).oneshot(request).await.unwrap()
}

#[cfg(target_os = "macos")]
fn owner_request(method: &str, path: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("x-wonder-loopback-capability", "test")
}

#[cfg(target_os = "macos")]
async fn text(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn socket_layout_finds_this_process_listener_only_inside_its_folder() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let here = crate::canonical_path::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let found = scan_listeners(std::slice::from_ref(&here), &[], deadline);
    let mine = found
        .iter()
        .find(|listener| listener.port == port)
        .unwrap_or_else(|| panic!("listener {port} not in {found:?}"));
    assert_eq!(mine.pid, std::process::id() as i32);
    assert_eq!(mine.address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(!mine.process_name.is_empty());
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = crate::canonical_path::canonicalize(elsewhere.path()).unwrap();
    assert!(scan_listeners(&[elsewhere], &[], deadline)
        .iter()
        .all(|listener| listener.port != port));
    // Excluded as another checkout nested in the folder.
    assert!(scan_listeners(
        std::slice::from_ref(&here),
        std::slice::from_ref(&here),
        deadline
    )
    .iter()
    .all(|listener| listener.port != port));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_dev_server_started_in_the_project_folder_is_listed_with_its_title() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    project_elsewhere(&state, "served", dir.path()).await;
    let folder = crate::canonical_path::canonicalize(dir.path().join("served")).unwrap();
    std::fs::write(
        folder.join("index.html"),
        "<title>Static Site</title><p>Hello</p>",
    )
    .unwrap();
    let mut child = tokio::process::Command::new("python3")
        .args([
            "-c",
            "import http.server,socketserver,sys\n\
             s=socketserver.TCPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler)\n\
             print(s.server_address[1],flush=True)\n\
             s.serve_forever()",
        ])
        .current_dir(&folder)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut line)
        .await
        .unwrap();
    let port: u16 = line.trim().parse().unwrap();
    // The test process's own listener is not in this Project's folder.
    let (own_port, own) = dev_server().await;
    let response = send(
        &state,
        owner_request("GET", "/api/v1/projects/served/preview/servers")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: Value = serde_json::from_str(&text(response).await).unwrap();
    crate::test_support::assert_http_contract("devServerList", &list);
    let servers = list["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "{list}");
    assert_eq!(servers[0]["port"], port);
    assert_eq!(servers[0]["title"], "Static Site");
    assert!(servers[0]["processName"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .contains("python"));
    let refused = send(
        &state,
        owner_request(
            "GET",
            &format!("/api/v1/projects/served/preview/{own_port}/"),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    child.kill().await.unwrap();
    own.abort();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn proxy_forwards_detected_ports_safely_and_refuses_the_rest() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    project_here(&state, "here").await;
    project_elsewhere(&state, "other", dir.path()).await;
    let (port, server) = dev_server().await;
    let base = format!("/api/v1/projects/here/preview/{port}");
    // An inspector-like JSON endpoint and a server with nothing at `/`.
    let not_pages = [
        Router::new().route(
            "/",
            get(|| async { Json(serde_json::json!({"Browser": "workerd"})) }),
        ),
        Router::new(),
    ];
    let mut other_ports = Vec::new();
    let mut other_tasks = Vec::new();
    for app in not_pages {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        other_ports.push(listener.local_addr().unwrap().port());
        other_tasks.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
    }

    let listed = send(
        &state,
        owner_request("GET", "/api/v1/projects/here/preview/servers")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let list: Value = serde_json::from_str(&text(listed).await).unwrap();
    let servers = list["servers"].as_array().unwrap();
    assert!(servers
        .iter()
        .any(|server| server["port"] == port && server["title"] == "Fixture App"));
    for other in &other_ports {
        assert!(
            servers.iter().all(|server| server["port"] != *other),
            "{list}"
        );
    }
    // A link naming one of them (an API with its docs below `/`) finds it.
    let linked = |port: u16| {
        let state = state.clone();
        async move {
            let response = send(
                &state,
                owner_request(
                    "GET",
                    &format!("/api/v1/projects/here/preview/servers?port={port}"),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await;
            let list: Value = serde_json::from_str(&text(response).await).unwrap();
            list["servers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|server| server["port"].as_u64().unwrap())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        linked(other_ports[1]).await,
        vec![u64::from(other_ports[1])]
    );
    assert_eq!(linked(port).await, vec![u64::from(port)]);
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let free = elsewhere.local_addr().unwrap().port();
    drop(elsewhere);
    assert!(linked(free).await.is_empty());
    for task in other_tasks {
        task.abort();
    }

    // Pages and root-relative assets.
    let page = send(
        &state,
        owner_request("GET", &format!("{base}/"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()[header::CONTENT_TYPE], "text/html");
    assert_eq!(page.headers()[SET_COOKIE_HEADER], "session=dev; Path=/");
    assert!(page.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(page.headers()[CSP_HEADER], "default-src 'self'");
    assert_eq!(page.headers()[UPSTREAM_HEADER], "1");
    assert!(text(page).await.contains("<title>Fixture App</title>"));
    let asset = send(
        &state,
        owner_request("GET", &format!("{base}/assets/app.js"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(text(asset).await, "console.log(1)");

    // Headers: only the page's own, with the dev server as host.
    let echoed = send(
        &state,
        owner_request("POST", &format!("{base}/echo?x=1"))
            .header("cookie", "__Host-wonder_session=secret")
            .header("x-wonder-csrf", "csrf")
            .header("referer", "https://wonder.test/")
            .header(COOKIE_HEADER, "session=dev")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(echoed.status(), StatusCode::OK);
    let echoed: Value = serde_json::from_str(&text(echoed).await).unwrap();
    assert_eq!(echoed["host"], format!("localhost:{port}"));
    assert_eq!(echoed["cookie"], "session=dev");
    assert_eq!(echoed["origin"], format!("http://localhost:{port}"));
    assert_eq!(echoed["content-type"], "application/json");
    for absent in [
        "x-wonder-csrf",
        "x-wonder-loopback-capability",
        "referer",
        COOKIE_HEADER,
    ] {
        assert!(
            echoed.get(absent).is_none(),
            "{absent} reached the dev server"
        );
    }

    // Redirects are reported, never followed.
    let redirected = send(
        &state,
        owner_request("GET", &format!("{base}/login-redirect"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(redirected.status(), StatusCode::FOUND);
    assert_eq!(redirected.headers()[LOCATION_HEADER], "/login");
    assert!(redirected.headers().get(header::LOCATION).is_none());
    let away = send(
        &state,
        owner_request("GET", &format!("{base}/away"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(away.status(), StatusCode::BAD_GATEWAY);
    assert!(away.headers().get(UPSTREAM_HEADER).is_none());
    assert_eq!(
        away.headers()[EXTERNAL_LOCATION_HEADER],
        "https://accounts.example.com/"
    );

    // Refusals: traversal, oversize uploads, another Project's view of the
    // same port, a port nothing listens on, and a missing session.
    for (method, path, body, status) in [
        (
            "GET",
            format!("{base}/a/%2e%2e/%2e%2e/x"),
            Body::empty(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("{base}/echo"),
            Body::from(vec![b'x'; MAX_REQUEST_BODY + 1]),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "GET",
            format!("/api/v1/projects/other/preview/{port}/"),
            Body::empty(),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/api/v1/projects/here/preview/1/".to_owned(),
            Body::empty(),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/api/v1/projects/missing/preview/1/".to_owned(),
            Body::empty(),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = send(&state, owner_request(method, &path).body(body).unwrap()).await;
        assert_eq!(response.status(), status, "{method} {path}");
    }
    let anonymous = send(
        &state,
        axum::http::Request::builder()
            .uri(format!("{base}/"))
            .header("host", "wonder.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong_origin = send(
        &state,
        axum::http::Request::builder()
            .uri(format!("{base}/"))
            .header("origin", format!("http://localhost:{port}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(wrong_origin.status(), StatusCode::FORBIDDEN);
    server.abort();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn hot_reload_sockets_pass_through_with_their_subprotocol() {
    let (_dir, state) = crate::ingestion::tests::fixture().await;
    project_here(&state, "sockets").await;
    let (port, server) = dev_server().await;
    let wonder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wonder_port = wonder.local_addr().unwrap().port();
    let app = crate::router(state.clone());
    let wonder_task = tokio::spawn(async move {
        axum::serve(wonder, app).await.unwrap();
    });
    let url = format!(
        "ws://127.0.0.1:{wonder_port}/api/v1/projects/sockets/preview/{port}/hmr?token=abc"
    );
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        )
        .unwrap();
    request.headers_mut().insert(
        "x-wonder-loopback-capability",
        HeaderValue::from_static("test"),
    );
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("vite-hmr"),
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()[header::SEC_WEBSOCKET_PROTOCOL],
        "vite-hmr"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::text("ping"))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.into_text().unwrap().as_str(), "echo:ping");

    // Without the owner's capability or a session there is no socket.
    let anonymous = tokio_tungstenite::connect_async(url.as_str()).await;
    assert!(anonymous.is_err());
    wonder_task.abort();
    server.abort();
}

#[cfg(target_os = "macos")]
fn git(dir: &Path, args: &[&str]) {
    let output = crate::test_git::git()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[cfg(target_os = "macos")]
async fn thread_in(state: &AppState, id: &str, project: &str, cwd: &Path) {
    state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: id,
            project_id: project,
            family: wonder_store::AgentFamily::Codex,
            provider_store: &state.projects.codex_store,
            native_session_id: None,
            cwd: cwd.to_str().unwrap(),
            roots_revision: 1,
            title: id,
            model: None,
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now: "now",
        })
        .await
        .unwrap();
}

/// A static site served by a separate process working in `folder`.
#[cfg(target_os = "macos")]
async fn static_site(folder: &Path, title: &str) -> (u16, tokio::process::Child) {
    std::fs::write(folder.join("index.html"), format!("<title>{title}</title>")).unwrap();
    let mut child = tokio::process::Command::new("python3")
        .args([
            "-c",
            "import http.server,socketserver,sys\n\
             s=socketserver.TCPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler)\n\
             print(s.server_address[1],flush=True)\n\
             s.serve_forever()",
        ])
        .current_dir(folder)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut line)
        .await
        .unwrap();
    (line.trim().parse().unwrap(), child)
}

#[cfg(target_os = "macos")]
async fn listed_ports(state: &AppState, path: &str) -> (StatusCode, Vec<u16>) {
    let response = send(
        state,
        owner_request("GET", path).body(Body::empty()).unwrap(),
    )
    .await;
    let status = response.status();
    let body = text(response).await;
    let ports = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|list| list["servers"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|server| server["port"].as_u64().unwrap() as u16)
        .collect();
    (status, ports)
}

// Contract: a thread sees the dev servers started in its own folder; the
// main checkout's thread does not see a server in a worktree nested inside
// it, the worktree's thread does not see the main checkout's, and the page
// proxy follows the same thread scope.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn each_thread_sees_only_its_own_folders_servers() {
    let (dir, state) = crate::ingestion::tests::fixture().await;
    project_elsewhere(&state, "app", dir.path()).await;
    project_elsewhere(&state, "other", dir.path()).await;
    let main = crate::canonical_path::canonicalize(dir.path().join("app")).unwrap();
    git(&main, &["init", "-q"]);
    git(
        &main,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    std::fs::create_dir_all(main.join(".local")).unwrap();
    git(
        &main,
        &["worktree", "add", "-qb", "agent/fix", ".local/wt-fix"],
    );
    let worktree = crate::canonical_path::canonicalize(main.join(".local/wt-fix")).unwrap();
    thread_in(&state, "main-thread", "app", &main).await;
    thread_in(&state, "worktree-thread", "app", &worktree).await;
    let other = crate::canonical_path::canonicalize(dir.path().join("other")).unwrap();
    thread_in(&state, "other-thread", "other", &other).await;
    let (main_port, mut main_server) = static_site(&main, "Main").await;
    let (worktree_port, mut worktree_server) = static_site(&worktree, "Fix").await;

    let list = "/api/v1/projects/app/preview/servers";
    let (status, ports) = listed_ports(&state, &format!("{list}?conversation=main-thread")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ports, vec![main_port]);
    let (_, ports) = listed_ports(&state, &format!("{list}?conversation=worktree-thread")).await;
    assert_eq!(ports, vec![worktree_port]);
    // Without a thread, the Project's folders as before.
    let (_, ports) = listed_ports(&state, list).await;
    assert!(
        ports.contains(&main_port) && ports.contains(&worktree_port),
        "{ports:?}"
    );
    // A thread of another Project, or none at all, names no scope here.
    for refused in ["other-thread", "missing"] {
        let (status, _) = listed_ports(&state, &format!("{list}?conversation={refused}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    }

    let page = |port: u16, thread: &str| {
        owner_request("GET", &format!("/api/v1/projects/app/preview/{port}/"))
            .header(CONVERSATION_HEADER, thread)
            .body(Body::empty())
            .unwrap()
    };
    let opened = send(&state, page(worktree_port, "worktree-thread")).await;
    assert_eq!(opened.status(), StatusCode::OK);
    assert!(text(opened).await.contains("<title>Fix</title>"));
    let refused = send(&state, page(worktree_port, "main-thread")).await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    let refused = send(&state, page(main_port, "other-thread")).await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);

    main_server.kill().await.unwrap();
    worktree_server.kill().await.unwrap();
}

#[test]
fn shared_fixture_matches_the_contract() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../packages/protocol/fixtures/dev-server-list-v1.json"
    ))
    .unwrap();
    crate::test_support::assert_http_contract("devServerList", &fixture);
}
