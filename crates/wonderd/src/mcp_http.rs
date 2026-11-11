//! Stateless MCP Streamable HTTP on the existing loopback listener. JSON
//! responses are the non-SSE variant of Streamable HTTP; GET explicitly has no
//! server event stream. OAuth tokens never authenticate Wonder's phone API.
use crate::mcp_oauth::{self as oauth, OAuthState, Urls};
use crate::thread_tools::{self, ToolError};
use crate::*;
use axum::http::HeaderMap;
use futures_util::StreamExt;
use reqwest::Url;
use serde_json::{json, Value};
use wonder_store::mcp::Grant;

pub(super) fn mount(router: Router<AppState>, state: AppState) -> Router<AppState> {
    let oauth = Arc::new(OAuthState::default());
    // Layer only these MethodRouters. Merging a separately layered Router
    // replaces Axum's default fallback, including the phone API's auth and
    // security headers for unknown paths.
    let secure = |methods: axum::routing::MethodRouter<AppState>| {
        methods
            .layer::<_, std::convert::Infallible>(DefaultBodyLimit::max(64 * 1024))
            .layer::<_, std::convert::Infallible>(Extension(oauth.clone()))
            .layer::<_, std::convert::Infallible>(middleware::from_fn_with_state(
                state.clone(),
                boundary,
            ))
    };
    router
        .route("/mcp", secure(post(mcp).get(no_stream).delete(no_stream)))
        .route(
            "/.well-known/oauth-protected-resource",
            secure(get(oauth::resource_metadata)),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            secure(get(oauth::resource_metadata)),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            secure(get(oauth::server_metadata)),
        )
        .route("/oauth/mcp/register", secure(post(oauth::register)))
        .route(
            "/oauth/mcp/authorize",
            secure(get(oauth::authorize).post(oauth::consent)),
        )
        .route("/oauth/mcp/token", secure(post(oauth::token)))
        .route(
            "/api/v1/mcp/pairing-code",
            secure(post(oauth::pairing_code)),
        )
        .route("/api/v1/mcp/clients", secure(get(oauth::clients)))
        .route(
            "/api/v1/mcp/clients/{id}",
            secure(axum::routing::delete(oauth::revoke)),
        )
}

/// Accept only the configured tailnet host, or an explicit loopback host for
/// local clients/fixtures. Never derive issuer or audience from Forwarded or
/// X-Forwarded-* headers, and never enable an existing public/Funnel hostname.
async fn boundary(State(app): State<AppState>, mut request: Request<Body>, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let local = Url::parse(&format!("http://{host}")).ok().filter(|u| {
        oauth::loopback(u)
            && u.path() == "/"
            && u.query().is_none()
            && u.username().is_empty()
            && u.password().is_none()
            && !host.contains(['/', '#', '?', '@'])
    });
    let issuer = if let Some(url) = local {
        url.origin().ascii_serialization()
    } else {
        let Some(origin) = current_public_origin(&app).await else {
            return oauth::failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "Cupola's tailnet address is not ready.",
            );
        };
        let Ok(url) = Url::parse(&origin) else {
            return StatusCode::FORBIDDEN.into_response();
        };
        let tailnet =
            url.scheme() == "https" && url.host_str().is_some_and(|h| h.ends_with(".ts.net"));
        if !tailnet || origin.strip_prefix("https://") != Some(host) {
            return StatusCode::FORBIDDEN.into_response();
        }
        origin
    };
    if request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|v| v.to_str().ok() != Some(issuer.as_str()))
    {
        return oauth::failure(StatusCode::FORBIDDEN, "access_denied", "Origin rejected.");
    }
    request.extensions_mut().insert(Urls {
        resource: format!("{issuer}/mcp"),
        issuer,
    });
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
        .headers_mut()
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ));
    response
}
fn challenge(urls: &Urls, status: StatusCode, scope: Option<&str>) -> Response {
    let mut response = oauth::failure(
        status,
        if scope.is_some() {
            "insufficient_scope"
        } else {
            "invalid_token"
        },
        "Sign in to Cupola with the required permissions.",
    );
    let mut value = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
        urls.issuer
    );
    if let Some(scope) = scope {
        value.push_str(&format!(
            ", error=\"insufficient_scope\", scope=\"{scope}\""
        ));
    } else {
        value.push_str(", error=\"invalid_token\"");
    }
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_str(&value).unwrap(),
    );
    response
}
// The error is the ready-to-send OAuth challenge; boxing it would only move the copy.
#[allow(clippy::result_large_err)]
async fn authenticate(app: &AppState, urls: &Urls, headers: &HeaderMap) -> Result<Grant, Response> {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token)
        .filter(|t| t.len() <= 256)
    else {
        return Err(challenge(urls, StatusCode::UNAUTHORIZED, None));
    };
    match app
        .store
        .mcp_access(&oauth::hash(token), &urls.resource, oauth::now())
        .await
    {
        Ok(Some(g)) => Ok(g),
        Ok(None) => Err(challenge(urls, StatusCode::UNAUTHORIZED, None)),
        Err(_) => Err(oauth::failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "Authorization could not be checked.",
        )),
    }
}
async fn no_stream(
    State(app): State<AppState>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authenticate(&app, &urls, &headers).await {
        return e;
    }
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST"));
    response
}
fn required_scope(tool: &str) -> Option<&'static str> {
    match tool {
        "wonder_project_list"
        | "wonder_provider_list"
        | "wonder_model_list"
        | thread_tools::LIST
        | thread_tools::READ
        | thread_tools::WAIT => Some("read"),
        "wonder_thread_start" => Some("start"),
        thread_tools::SEND => Some("message"),
        thread_tools::STOP => Some("stop"),
        name if SIMULATOR_TOOLS.contains(&name) => Some("simulator"),
        _ => None,
    }
}
const SIM_LIST: &str = "wonder_simulator_list";
const SIM_SETTINGS: &str = "wonder_simulator_settings";
const SIM_SCREENSHOT: &str = "wonder_simulator_screenshot";
const SIM_APPS: &str = "wonder_simulator_apps";
const SIM_APPEARANCE: &str = "wonder_simulator_set_appearance";
const SIM_CONTENT_SIZE: &str = "wonder_simulator_set_content_size";
const SIM_LANGUAGE: &str = "wonder_simulator_set_language";
const SIM_RESET: &str = "wonder_simulator_reset_permissions";
const SIM_OPEN_URL: &str = "wonder_simulator_open_url";
const SIMULATOR_TOOLS: [&str; 9] = [
    SIM_LIST,
    SIM_SETTINGS,
    SIM_SCREENSHOT,
    SIM_APPS,
    SIM_APPEARANCE,
    SIM_CONTENT_SIZE,
    SIM_LANGUAGE,
    SIM_RESET,
    SIM_OPEN_URL,
];
fn read_only(tool: &str) -> bool {
    required_scope(tool) == Some("read")
        || [SIM_LIST, SIM_SETTINGS, SIM_SCREENSHOT, SIM_APPS].contains(&tool)
}
fn simulator_specs() -> Vec<Value> {
    use crate::simulators::{APPEARANCES, CONTENT_SIZES, MAX_URL_BYTES};
    let udid = json!({"type":"string","pattern":"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$","description":"A udid from wonder_simulator_list."});
    let object = |mut properties: Value, required: &[&str]| {
        properties["udid"] = udid.clone();
        let mut required: Vec<&str> = required.to_vec();
        required.insert(0, "udid");
        json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
    };
    let spec = |name: &str, description: &str, schema: Value| json!({"name":name,"description":description,"inputSchema":schema});
    vec![
        spec(SIM_LIST,"List this computer's simulators (name, iOS version, state, device type) and the computer's name. Only booted simulators take actions; agents cannot tap or type in them. Read-only.",json!({"type":"object","additionalProperties":false,"properties":{},"required":[]})),
        spec(SIM_SETTINGS,"Read a booted simulator's appearance, text size, language and locale. Read-only.",object(json!({}),&[])),
        spec(SIM_SCREENSHOT,"Take a PNG screenshot of a booted simulator's screen. Read-only.",object(json!({}),&[])),
        spec(SIM_APPS,"List the apps installed by the owner in a booted simulator, for wonder_simulator_reset_permissions. Read-only.",object(json!({}),&[])),
        spec(SIM_APPEARANCE,"Switch a booted simulator to Light or Dark appearance; returns the value read back.",object(json!({"appearance":{"type":"string","enum":APPEARANCES}}),&["appearance"])),
        spec(SIM_CONTENT_SIZE,"Set a booted simulator's preferred text size (Dynamic Type); returns the value read back.",object(json!({"contentSize":{"type":"string","enum":CONTENT_SIZES}}),&["contentSize"])),
        spec(SIM_LANGUAGE,"Set a booted simulator's language and region, e.g. fr-FR. Apps use it when they next open; the system after the simulator restarts, which this tool does not do.",object(json!({"language":{"type":"string","maxLength":16,"pattern":"^[a-z]{2,3}(-[A-Z][a-z]{3})?(-([A-Z]{2}|[0-9]{3}))?$"}}),&["language"])),
        spec(SIM_RESET,"Reset every privacy permission of an installed app in a booted simulator so it asks again. The app may quit.",object(json!({"bundleId":{"type":"string","maxLength":155}}),&["bundleId"])),
        spec(SIM_OPEN_URL,"Open an http(s) link or an app link (myapp://...) in a booted simulator. File, script and data links are refused.",object(json!({"url":{"type":"string","minLength":1,"maxLength":MAX_URL_BYTES}}),&["url"])),
    ]
}
fn specs(grant: &Grant) -> Vec<Value> {
    let mut specs = thread_tools::specs()
        .into_iter()
        .filter(|s| {
            matches!(
                s["name"].as_str(),
                Some(thread_tools::LIST | thread_tools::READ | thread_tools::SEND)
            )
        })
        .collect::<Vec<_>>();
    for spec in &mut specs {
        spec.as_object_mut().unwrap().remove("type");
        if spec["name"] == thread_tools::LIST {
            spec["inputSchema"]["properties"]["projectId"] = json!({"type":"string"});
            spec["inputSchema"]["required"] = json!(["projectId"]);
            spec["description"] = json!("List threads in an included Project. Read-only.");
        } else if spec["name"] == thread_tools::SEND {
            spec["description"]=json!("Send or steer a thread in an included Project. Requires message scope; the thread cannot have wider permissions than this grant. Reuse clientRequestId for retries. This can start an agent on the computer.");
        } else {
            spec["description"]=json!("Read a thread in any included Project. Returns messages and short activity summaries, without raw tool output. Read-only.");
        }
    }
    let object = |properties: Value, required: Value| json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
    let spec = |name: &str, description: &str, schema: Value| json!({"name":name,"description":description,"inputSchema":schema});
    specs.extend([
        spec("wonder_project_list","List included Projects. Read-only. A Project in a Git repository includes `repository` {key, subpath, name}: the same key and subpath on another computer is the same checkout location, for grouping only.",object(json!({}),json!([]))),
        spec("wonder_provider_list","List Cupola providers and whether they are installed. Read-only.",object(json!({}),json!([]))),
        spec("wonder_model_list","List the cached provider model catalog. Read-only.",object(json!({"provider":{"type":"string"}}),json!([]))),
        spec("wonder_thread_start","Start a new thread and give it its first message in an included Project. Requires start scope. The thread cannot exceed this grant's maximum permissions; Workspace always asks for approval. Reuse clientRequestId for retries.",object(json!({"projectId":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},"accessMode":{"type":"string","enum":["read_only","workspace","full_access"]},"prompt":{"type":"string","maxLength":32768},"clientRequestId":{"type":"string","format":"uuid"}}),json!(["projectId","provider","model","prompt","clientRequestId"]))),
        spec(thread_tools::STOP,"Stop a thread's current response. Requires stop scope and permission to direct that thread.",object(json!({"threadId":{"type":"string"}}),json!(["threadId"]))),
        spec(thread_tools::WAIT,"Wait up to 30 seconds for a thread to finish. Read-only; a timeout does not stop it.",object(json!({"threadId":{"type":"string"},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":30}}),json!(["threadId"]))),
    ]);
    specs.extend(simulator_specs());
    specs.retain(|s| {
        required_scope(s["name"].as_str().unwrap())
            .is_some_and(|scope| oauth::has_scope(grant, scope))
    });
    for spec in &mut specs {
        spec["annotations"] = json!({"readOnlyHint": read_only(spec["name"].as_str().unwrap())});
    }
    specs
}
fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}
async fn mcp(
    State(app): State<AppState>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let grant = match authenticate(&app, &urls, &headers).await {
        Ok(g) => g,
        Err(e) => return e,
    };
    if headers.get("mcp-protocol-version").is_some_and(|v| {
        !matches!(
            v.to_str().ok(),
            Some("2025-03-26" | "2025-06-18" | "2025-11-25")
        )
    }) {
        return oauth::failure(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Unsupported MCP protocol version.",
        );
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request["jsonrpc"] != "2.0"
        || !request.is_object()
        || request
            .get("id")
            .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
    {
        return rpc_error(Value::Null, -32600, "Invalid JSON-RPC request.");
    }
    let Some(method) = request["method"].as_str() else {
        return rpc_error(id, -32600, "A method is required.");
    };
    if request.get("id").is_none() {
        return if method.starts_with("notifications/") {
            StatusCode::ACCEPTED.into_response()
        } else {
            rpc_error(Value::Null, -32600, "Only notifications may omit id.")
        };
    }
    let result = match method {
        "initialize" => {
            let version = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = if ["2025-03-26", "2025-06-18", "2025-11-25"].contains(&version) {
                version
            } else {
                "2025-06-18"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"Cupola","version":crate::RELEASE_VERSION}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":specs(&grant)}),
        "tools/call" => {
            let Some(tool) = request.pointer("/params/name").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "Tool name is required.");
            };
            let Some(scope) = required_scope(tool) else {
                return rpc_error(id, -32602, "Unknown tool.");
            };
            if !oauth::has_scope(&grant, scope) {
                return challenge(&urls, StatusCode::FORBIDDEN, Some(scope));
            }
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let spec = specs(&grant)
                .into_iter()
                .find(|s| s["name"] == tool)
                .unwrap();
            let validator = jsonschema::validator_for(&spec["inputSchema"]).unwrap();
            if !validator.is_valid(&args) {
                return rpc_error(id, -32602, "Arguments do not match the tool schema.");
            }
            match call(&app, &grant, tool, args, &urls, &headers).await {
                Ok(mut value) if tool == SIM_SCREENSHOT => {
                    // The image as MCP image content; the summary without its bytes.
                    let data = value.as_object_mut().unwrap().remove("imageBase64");
                    json!({"content":[{"type":"image","data":data,"mimeType":"image/png"},{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":json!({"error":{"code":error.code,"message":error.message}}).to_string()}],"isError":true})
                }
            }
        }
        _ => return rpc_error(id, -32601, "Method not found."),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
fn invalid(message: &str) -> ToolError {
    ToolError::new("invalid", message)
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 256)
        .ok_or_else(|| invalid("A valid Project, thread or provider ID is required."))
}
async fn project(app: &AppState, id: &str) -> Result<wonder_store::StoredProject, ToolError> {
    app.store
        .project(id)
        .await?
        .filter(|p| p.is_included)
        .ok_or_else(|| ToolError::new("not_found", "No such included Project."))
}
async fn thread(
    app: &AppState,
    args: &Value,
) -> Result<
    (
        wonder_store::StoredProject,
        wonder_store::StoredProjectConversation,
    ),
    ToolError,
> {
    let thread = app
        .store
        .project_conversation(text(args, "threadId")?)
        .await?
        .ok_or_else(|| ToolError::new("not_found", "No such thread."))?;
    Ok((project(app, &thread.project_id).await?, thread))
}
async fn response_value(response: Response) -> Result<Value, ToolError> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(|_| invalid("The result was too large."))?;
    if !status.is_success() {
        return Err(ToolError::new(
            "unavailable",
            crate::history::sanitize_ansi_and_secrets(&String::from_utf8_lossy(&bytes), 2000),
        ));
    }
    Ok(serde_json::from_slice(&bytes).unwrap_or(json!({"stopped":true})))
}
async fn call(
    app: &AppState,
    grant: &Grant,
    tool: &str,
    mut args: Value,
    urls: &Urls,
    headers: &HeaderMap,
) -> Result<Value, ToolError> {
    match tool {
        "wonder_project_list" => {
            let included: Vec<_> = app
                .store
                .list_projects()
                .await?
                .into_iter()
                .filter(|p| p.is_included)
                .collect();
            // The repository groups the same checkout across computers; it
            // never stands in for the Project ID when choosing a target.
            let projects: Vec<Value> = futures_util::stream::iter(included)
                .map(|p| async move {
                    let mut entry = json!({"id":p.id,"name":p.name});
                    if let Some(repository) = crate::projects::identity::repository(&p).await {
                        entry["repository"] = json!(repository);
                    }
                    entry
                })
                .buffered(crate::projects::identity::CONCURRENCY)
                .collect()
                .await;
            Ok(json!({"projects":projects}))
        }
        "wonder_provider_list" => Ok(
            json!({"providers":AgentFamily::ALL.into_iter().map(|f|json!({"id":f.as_str(),"name":f.provider().display_name,"available":crate::claude::available(app,f)})).collect::<Vec<_>>()}),
        ),
        "wonder_model_list" => {
            let provider = args["provider"].as_str();
            let family = AgentFamily::ALL
                .into_iter()
                .find(|f| Some(f.as_str()) == provider);
            if provider.is_some() && family.is_none() {
                return Ok(json!({"models":[]}));
            }
            let catalog = app.runtime_catalog.read().await;
            Ok(json!({"models":thread_tools::listed_models(&catalog, family).collect::<Vec<_>>()}))
        }
        thread_tools::LIST => {
            let p = project(app, text(&args, "projectId")?).await?;
            args.as_object_mut().unwrap().remove("projectId");
            thread_tools::outside_call(app, grant, p, tool, args).await
        }
        thread_tools::READ | thread_tools::SEND => {
            let (p, _) = thread(app, &args).await?;
            thread_tools::outside_call(app, grant, p, tool, args).await
        }
        "wonder_thread_start" => start(app, grant, args).await,
        name if SIMULATOR_TOOLS.contains(&name) => simulator(app, name, &args).await,
        thread_tools::STOP => {
            let (_, t) = thread(app, &args).await?;
            thread_tools::outside_authority(grant, &t)?;
            thread_tools::stop_running(app, &t.conversation_id).await
        }
        thread_tools::WAIT => {
            let (_, t) = thread(app, &args).await?;
            let duration = args["timeoutSeconds"].as_u64().unwrap_or(10);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(duration);
            loop {
                if !authenticate(app, urls, headers)
                    .await
                    .is_ok_and(|g| oauth::has_scope(&g, "read"))
                {
                    return Err(ToolError::new(
                        "unauthorized",
                        "Authorization expired or was revoked.",
                    ));
                }
                let run = app.store.thread_run_state(&t.conversation_id).await?;
                let working = run.working || run.waiting || run.queued > 0;
                if !working || tokio::time::Instant::now() >= deadline {
                    return Ok(
                        json!({"threadId":t.conversation_id,"status":if working{"still_running"}else{"idle"}}),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        _ => Err(invalid("Unknown tool.")),
    }
}
/// The phone's typed simulator actions (`simulators`), for agents. Input is
/// not offered: it needs the phone's control lease.
async fn simulator(app: &AppState, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let s = &app.simulators;
    let arg = |key: &str| args[key].as_str().unwrap_or_default();
    let udid = arg("udid");
    let result = match tool {
        SIM_LIST => {
            let mut list = crate::simulators::list_json(app).await;
            list["agentInput"] =
                json!({"available":false,"reason":crate::simulators::AGENT_INPUT_UNAVAILABLE});
            return Ok(list);
        }
        SIM_SETTINGS => s.settings(udid).await,
        SIM_SCREENSHOT => s
            .screenshot(udid)
            .await
            .map(|shot| shot.json(&udid.to_ascii_uppercase())),
        SIM_APPS => s.apps(udid).await,
        SIM_APPEARANCE => s.set_appearance(udid, arg("appearance")).await,
        SIM_CONTENT_SIZE => s.set_content_size(udid, arg("contentSize")).await,
        SIM_LANGUAGE => s.set_language(udid, arg("language")).await,
        SIM_RESET => s.reset_permissions(udid, arg("bundleId")).await,
        SIM_OPEN_URL => s.open_url(udid, arg("url")).await,
        _ => return Err(invalid("Unknown tool.")),
    };
    result.map_err(|error| ToolError::new(error.code(), error.message()))
}
async fn start(app: &AppState, grant: &Grant, args: Value) -> Result<Value, ToolError> {
    let project = project(app, text(&args, "projectId")?).await?;
    let family = AgentFamily::ALL
        .into_iter()
        .find(|f| Some(f.as_str()) == args["provider"].as_str())
        .ok_or_else(|| invalid("Unknown provider."))?;
    let access = args["accessMode"]
        .as_str()
        .unwrap_or(&grant.permission_mode);
    let rank = |s: &str| match s {
        "read_only" => 0,
        "workspace" => 1,
        _ => 2,
    };
    if rank(access) > rank(&grant.permission_mode)
        || (!family.provider().capabilities.access_choices
            && grant.permission_mode != "full_access")
    {
        return Err(ToolError::new(
            "permission_escalation",
            "The thread cannot exceed the approved permission mode.",
        ));
    }
    let access = if family.provider().capabilities.access_choices {
        access
    } else {
        "workspace"
    };
    // Namespace client action UUIDs so two clients can never replay each other's requests.
    let request = text(&args, "clientRequestId")?;
    uuid::Uuid::parse_str(request).map_err(|_| invalid("clientRequestId must be a UUID."))?;
    let digest = Sha256::digest(format!("mcp:{}:{request}", grant.client_id).as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let request = uuid::Uuid::from_bytes(bytes).to_string();
    app.store
        .ensure_local_desktop(&Utc::now().to_rfc3339())
        .await?;
    // Reserve through the existing creation handler, then use the existing
    // send handler for durable, labelled, idempotent first-message acceptance.
    let create:projects::CreateThreadRequest=serde_json::from_value(json!({
        "deviceId":"wonder-desktop","clientMessageId":request,"family":family.as_str(),"model":args["model"],"effort":args["effort"],"accessMode":access,"claudeApproval":if family.allows_approval_choice(){Some("ask")}else{None},"body":args["prompt"],"prepareOnly":true
    })).map_err(|_|invalid("Invalid thread settings."))?;
    let result = response_value(
        projects::create_thread(
            State(app.clone()),
            Extension(OwnerAuthority),
            None,
            Path(project.id.clone()),
            Json(create),
        )
        .await,
    )
    .await?;
    let id = result
        .pointer("/conversation/id")
        .or_else(|| result.pointer("/conversation/conversationId"))
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new("unavailable", "The created thread ID was missing."))?;
    let sent = thread_tools::outside_call(
        app,
        grant,
        project,
        thread_tools::SEND,
        json!({"threadId":id,"clientRequestId":request,"message":args["prompt"]}),
    )
    .await?;
    Ok(json!({"threadId":id,"message":sent}))
}
