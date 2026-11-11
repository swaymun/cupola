//! OAuth 2.1 authorization-code + S256 PKCE for outside agents. Approval always
//! needs a one-use code obtained through the Mac's local capability.
use crate::*;
use axum::{extract::Form, http::HeaderMap, response::Html};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use wonder_store::mcp::{AuthorizationCode, Client, Grant, TokenHashes};

pub(super) const ACCESS_MS: i64 = 10 * 60 * 1000;
pub(super) const GRANT_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const CODE_MS: i64 = 60 * 1000;
const PAIRING_MS: i64 = 5 * 60 * 1000;
pub(super) const SCOPES: [&str; 5] = ["read", "start", "message", "stop", "simulator"];

#[derive(Clone)]
pub(super) struct Urls {
    pub issuer: String,
    pub resource: String,
}
#[derive(Default)]
pub(super) struct OAuthState {
    pending: tokio::sync::Mutex<HashMap<String, Pending>>,
    rates: std::sync::Mutex<HashMap<&'static str, VecDeque<i64>>>,
}
impl OAuthState {
    pub fn allow(&self, bucket: &'static str, limit: usize) -> bool {
        let now = now();
        let mut rates = self.rates.lock().unwrap();
        let times = rates.entry(bucket).or_default();
        while times.front().is_some_and(|at| *at <= now - 60_000) {
            times.pop_front();
        }
        if times.len() >= limit {
            return false;
        }
        times.push_back(now);
        true
    }
}
#[derive(Clone)]
struct Pending {
    client: Client,
    redirect: String,
    challenge: String,
    state: Option<String>,
    csrf_hash: String,
    scopes: String,
    urls: Urls,
    expires: i64,
}

pub(super) fn now() -> i64 {
    now_ms() as i64
}
pub(super) fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub(super) fn secret() -> String {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .expect("system randomness unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}
pub(super) fn failure(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({"error":error,"error_description":description})),
    )
        .into_response()
}
fn invalid(message: &str) -> Response {
    failure(StatusCode::BAD_REQUEST, "invalid_request", message)
}
fn unavailable() -> Response {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "server_error",
        "Cupola could not save authorization. Try again.",
    )
}
pub(super) fn has_scope(grant: &Grant, scope: &str) -> bool {
    grant.scopes.split_whitespace().any(|s| s == scope)
}
fn scopes(value: Option<&str>) -> Option<String> {
    let requested: Vec<_> = value.unwrap_or("read").split_whitespace().collect();
    if requested.is_empty() || requested.iter().any(|s| !SCOPES.contains(s)) {
        return None;
    }
    Some(
        SCOPES
            .into_iter()
            .filter(|s| requested.contains(s))
            .collect::<Vec<_>>()
            .join(" "),
    )
}
pub(super) fn valid_redirect(value: &str) -> Option<Url> {
    if value.len() > 2048 || value.contains('#') || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return None;
    }
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback(&url)) {
        return None;
    }
    if url
        .query_pairs()
        .any(|(k, _)| ["code", "state", "error", "iss"].contains(&k.as_ref()))
    {
        return None;
    }
    Some(url)
}
pub(super) fn loopback(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}
pub(super) fn redirect_matches(registered: &str, presented: &str) -> bool {
    match (valid_redirect(registered), valid_redirect(presented)) {
        (Some(mut a), Some(mut b)) => {
            if a.scheme() == "http" && b.scheme() == "http" {
                let _ = a.set_port(None);
                let _ = b.set_port(None);
            }
            a == b
        }
        _ => false,
    }
}
fn same_resource(value: Option<&str>, urls: &Urls) -> bool {
    value.is_none_or(|value| value == urls.resource)
}
fn pkce_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn pkce_verifies(verifier: &str, expected: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
        && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == expected
}

pub(super) async fn resource_metadata(Extension(urls): Extension<Urls>) -> Json<Value> {
    Json(
        json!({"resource":urls.resource,"authorization_servers":[urls.issuer],"scopes_supported":SCOPES,"bearer_methods_supported":["header"],"resource_name":"Cupola"}),
    )
}
pub(super) async fn server_metadata(Extension(urls): Extension<Urls>) -> Json<Value> {
    Json(
        json!({"issuer":urls.issuer,"authorization_endpoint":format!("{}/oauth/mcp/authorize",urls.issuer),"token_endpoint":format!("{}/oauth/mcp/token",urls.issuer),"registration_endpoint":format!("{}/oauth/mcp/register",urls.issuer),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"],"scopes_supported":SCOPES,"authorization_response_iss_parameter_supported":true}),
    )
}
#[derive(Deserialize)]
pub(super) struct Registration {
    client_name: Option<String>,
    redirect_uris: Vec<String>,
}
pub(super) async fn register(
    State(app): State<AppState>,
    Extension(oauth): Extension<Arc<OAuthState>>,
    Json(request): Json<Registration>,
) -> Response {
    if !oauth.allow("register", 30) {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Try again in a minute.",
        );
    }
    if request.redirect_uris.is_empty()
        || request.redirect_uris.len() > 5
        || request
            .redirect_uris
            .iter()
            .any(|u| valid_redirect(u).is_none())
    {
        return failure(StatusCode::BAD_REQUEST,"invalid_redirect_uri","Register 1 to 5 HTTPS or HTTP loopback redirect URLs, without credentials or fragments.");
    }
    let name = request
        .client_name
        .as_deref()
        .unwrap_or("MCP client")
        .trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return invalid("Use a client name of 1 to 100 characters.");
    }
    let client = Client {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.into(),
        redirect_uris: request.redirect_uris,
        created_at_ms: now(),
        revoked_at_ms: None,
    };
    match app.store.mcp_register_client(&client).await {
        Ok(true) => (StatusCode::CREATED,Json(json!({"client_id":client.id,"client_name":client.name,"redirect_uris":client.redirect_uris,"client_id_issued_at":client.created_at_ms/1000,"token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"],"response_types":["code"]}))).into_response(),
        Ok(false) => failure(StatusCode::TOO_MANY_REQUESTS,"temporarily_unavailable","Too many registered clients. Remove unused clients on the computer."),
        Err(_) => unavailable(),
    }
}
#[derive(Deserialize)]
pub(super) struct Authorization {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub(super) async fn authorize(
    State(app): State<AppState>,
    Extension(oauth): Extension<Arc<OAuthState>>,
    Extension(urls): Extension<Urls>,
    Query(request): Query<Authorization>,
) -> Response {
    if !oauth.allow("authorize", 60) {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Try again in a minute.",
        );
    }
    let client = match app.store.mcp_client(&request.client_id).await {
        Ok(Some(client)) if client.revoked_at_ms.is_none() => client,
        Ok(_) => return invalid("Unknown or revoked agent. Start sign-in again from the agent."),
        Err(_) => return unavailable(),
    };
    if !client
        .redirect_uris
        .iter()
        .any(|u| redirect_matches(u, &request.redirect_uri))
    {
        return invalid("The return address was not registered by this agent.");
    }
    if request.response_type != "code"
        || request.code_challenge_method != "S256"
        || !pkce_challenge(&request.code_challenge)
    {
        return invalid("Authorization code with S256 PKCE is required.");
    }
    if !same_resource(request.resource.as_deref(), &urls) {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "This authorization is for a different MCP endpoint.",
        );
    }
    if request.state.as_ref().is_some_and(|s| s.len() > 2048) {
        return invalid("State is too long.");
    }
    let Some(scopes) = scopes(request.scope.as_deref()) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "Use read, start, message or stop.",
        );
    };
    let id = secret();
    let csrf = secret();
    let pending = Pending {
        client: client.clone(),
        redirect: request.redirect_uri.clone(),
        challenge: request.code_challenge,
        state: request.state,
        csrf_hash: hash(&csrf),
        scopes: scopes.clone(),
        urls: urls.clone(),
        expires: now() + PAIRING_MS,
    };
    let mut requests = oauth.pending.lock().await;
    requests.retain(|_, p| p.expires > now());
    if requests.len() >= 256 {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "temporarily_unavailable",
            "Too many pending sign-ins. Try again later.",
        );
    }
    requests.insert(hash(&id), pending);
    drop(requests);
    let target = valid_redirect(&request.redirect_uri).unwrap();
    let explanation = if loopback(&target) {
        "Approval returns to localhost on the computer where you open this page. For a headless agent on another computer, paste the final return URL back into its terminal."
    } else {
        "Approval gives access to the service at this address. Its operator can use the granted access."
    };
    let mut choices = String::new();
    for (scope, label) in [
        ("start", "Start threads"),
        ("message", "Send messages and steer threads"),
        ("stop", "Stop threads"),
        ("simulator", "Use this computer's running simulators: take screenshots, change appearance, text size and language, open links and reset app permissions (not tap or type)"),
    ] {
        if scopes.split_whitespace().any(|s| s == scope) {
            choices.push_str(&format!("<p><label><input type=\"checkbox\" name=\"allow_{scope}\" value=\"yes\"> {label}</label></p>"));
        }
    }
    let html = format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Connect to Cupola</title><body><main><h1>Connect {} to Cupola</h1><p>Only approve a sign-in you just started.</p><p>Returns to <strong>{}</strong></p><p>{}</p><form method="post" action="/oauth/mcp/authorize"><input type="hidden" name="request_id" value="{}"><input type="hidden" name="csrf" value="{}"><p><label>Pairing code shown on your computer <input name="pairing_code" autocomplete="off" required maxlength="32"></label></p><p>Read projects, threads, providers and models. Read only is the default.</p>{}<p><label>Maximum thread permissions <select name="permission_mode"><option value="read_only" selected>Read only</option><option value="workspace">Workspace, ask before acting</option><option value="full_access">Full access</option></select></label></p><p>Permissions apply to all included Projects. Starting or messaging threads can run an agent on your computer.</p><button name="decision" value="approve">Approve</button> <button name="decision" value="deny" formnovalidate>Deny</button></form></main></body></html>"#,
        escape(&client.name),
        escape(&request.redirect_uri),
        explanation,
        id,
        csrf,
        choices
    );
    let mut response = Html(html).into_response();
    // Permit the registered callback after form submission; browsers can
    // enforce form-action across the OAuth redirect as well as the POST.
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&format!(
            "default-src 'none'; form-action 'self' {}; frame-ancestors 'none'; base-uri 'none'",
            target.origin().ascii_serialization()
        ))
        .unwrap(),
    );
    let secure = if urls.issuer.starts_with("https:") {
        "; Secure"
    } else {
        ""
    };
    response.headers_mut().insert(header::SET_COOKIE,HeaderValue::from_str(&format!("wonder_mcp_consent={csrf}; HttpOnly; SameSite=Strict; Path=/oauth/mcp/authorize; Max-Age=300{secure}")).unwrap());
    response
}
#[derive(Deserialize)]
pub(super) struct Consent {
    request_id: String,
    csrf: String,
    pairing_code: Option<String>,
    permission_mode: Option<String>,
    allow_start: Option<String>,
    allow_message: Option<String>,
    allow_stop: Option<String>,
    allow_simulator: Option<String>,
    decision: String,
}
fn redirect(p: &Pending, code: Option<&str>) -> Response {
    let mut url = Url::parse(&p.redirect).unwrap();
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(code) = code {
            pairs.append_pair("code", code);
        } else {
            pairs.append_pair("error", "access_denied");
        }
        if let Some(state) = &p.state {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", &p.urls.issuer);
    }
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(url.as_str()).unwrap(),
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "wonder_mcp_consent=; HttpOnly; SameSite=Strict; Path=/oauth/mcp/authorize; Max-Age=0",
        ),
    );
    response
}
pub(super) async fn consent(
    State(app): State<AppState>,
    Extension(oauth): Extension<Arc<OAuthState>>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
    Form(form): Form<Consent>,
) -> Response {
    if !oauth.allow("consent", 10) {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Too many pairing attempts. Try again in a minute.",
        );
    }
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .map(str::trim)
                .find_map(|v| v.strip_prefix("wonder_mcp_consent="))
        });
    let mut requests = oauth.pending.lock().await;
    let key = hash(&form.request_id);
    let Some(pending) = requests.get(&key) else {
        return invalid("Sign-in expired. Start again from the agent.");
    };
    if pending.expires <= now()
        || pending.urls.resource != urls.resource
        || cookie != Some(form.csrf.as_str())
        || hash(&form.csrf) != pending.csrf_hash
    {
        return failure(
            StatusCode::FORBIDDEN,
            "access_denied",
            "Sign-in verification failed. Start again from the agent.",
        );
    }
    if form.decision == "deny" {
        let p = requests.remove(&key).unwrap();
        return redirect(&p, None);
    }
    if form.decision != "approve" {
        return invalid("Choose Approve or Deny.");
    }
    let mode = form.permission_mode.as_deref().unwrap_or("read_only");
    if !["read_only", "workspace", "full_access"].contains(&mode) {
        return invalid("Unknown permission mode.");
    }
    let mut allowed = vec!["read"];
    for (scope, choice) in [
        ("start", &form.allow_start),
        ("message", &form.allow_message),
        ("stop", &form.allow_stop),
        ("simulator", &form.allow_simulator),
    ] {
        if choice.as_deref() == Some("yes") {
            if !pending.scopes.split_whitespace().any(|s| s == scope) {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "invalid_scope",
                    "Approval cannot add an unrequested scope.",
                );
            }
            allowed.push(scope);
        }
    }
    if !pending.scopes.split_whitespace().any(|s| s == "read") {
        allowed.retain(|s| *s != "read");
    }
    let code = secret();
    let record = AuthorizationCode {
        hash: hash(&code),
        client_id: pending.client.id.clone(),
        redirect_uri: pending.redirect.clone(),
        code_challenge: pending.challenge.clone(),
        audience: urls.resource,
        scopes: allowed.join(" "),
        permission_mode: mode.into(),
        expires_at_ms: now() + CODE_MS,
    };
    let pairing = form
        .pairing_code
        .as_deref()
        .unwrap_or("")
        .trim()
        .replace('-', "")
        .to_ascii_uppercase();
    match app.store.mcp_approve(&hash(&pairing), &record, now()).await {
        Ok(true) => {
            let p = requests.remove(&key).unwrap();
            redirect(&p, Some(&code))
        }
        Ok(false) => failure(
            StatusCode::FORBIDDEN,
            "access_denied",
            "The pairing code is wrong, expired or already used. Get a fresh code on the computer.",
        ),
        Err(_) => unavailable(),
    }
}
#[derive(Deserialize)]
pub(super) struct TokenRequest {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}
pub(super) async fn token(
    State(app): State<AppState>,
    Extension(oauth): Extension<Arc<OAuthState>>,
    Extension(urls): Extension<Urls>,
    Form(request): Form<TokenRequest>,
) -> Response {
    if !oauth.allow("token", 120) {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Try again in a minute.",
        );
    }
    let fail = || {
        failure(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "The credential is invalid, expired or revoked.",
        )
    };
    let access = secret();
    let refresh = secret();
    let ah = hash(&access);
    let rh = hash(&refresh);
    let now = now();
    let hashes = TokenHashes {
        access: &ah,
        refresh: &rh,
        access_expires_at_ms: now + ACCESS_MS,
    };
    let grant = match request.grant_type.as_str() {
        "authorization_code" => {
            let Some(raw) = request.code.as_deref() else {
                return invalid("An authorization code is required.");
            };
            let code = match app.store.mcp_take_code(&hash(raw)).await {
                Ok(Some(c)) => c,
                Ok(None) => return fail(),
                Err(_) => return unavailable(),
            };
            if code.expires_at_ms <= now
                || code.client_id != request.client_id
                || code.audience != urls.resource
                || !same_resource(request.resource.as_deref(), &urls)
                || request.redirect_uri.as_deref() != Some(code.redirect_uri.as_str())
                || !request
                    .code_verifier
                    .as_deref()
                    .is_some_and(|v| pkce_verifies(v, &code.code_challenge))
            {
                return fail();
            }
            if request.scope.as_deref().is_some_and(|s| s != code.scopes) {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "invalid_scope",
                    "The code's approved scopes cannot be changed.",
                );
            }
            let g = Grant {
                id: uuid::Uuid::new_v4().to_string(),
                client_id: code.client_id,
                client_name: String::new(),
                audience: code.audience,
                scopes: code.scopes,
                permission_mode: code.permission_mode,
                expires_at_ms: now + GRANT_MS,
            };
            match app.store.mcp_issue_grant(&g, &hashes).await {
                Ok(true) => g,
                Ok(false) => return fail(),
                Err(_) => return unavailable(),
            }
        }
        "refresh_token" => {
            if !same_resource(request.resource.as_deref(), &urls) {
                return fail();
            }
            let Some(raw) = request.refresh_token.as_deref() else {
                return invalid("A refresh token is required.");
            };
            let requested = match request.scope.as_deref() {
                Some(s) => match scopes(Some(s)) {
                    Some(s) => Some(s),
                    None => {
                        return failure(StatusCode::BAD_REQUEST, "invalid_scope", "Unknown scope.")
                    }
                },
                None => None,
            };
            match app
                .store
                .mcp_refresh(
                    &hash(raw),
                    &request.client_id,
                    &urls.resource,
                    now,
                    &hashes,
                    requested.as_deref(),
                )
                .await
            {
                Ok(Some(g)) => g,
                Ok(None) => return fail(),
                Err(_) => return unavailable(),
            }
        }
        _ => {
            return failure(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "Use authorization_code or refresh_token.",
            )
        }
    };
    Json(json!({"access_token":access,"refresh_token":refresh,"token_type":"Bearer","expires_in":(grant.expires_at_ms.min(now+ACCESS_MS)-now)/1000,"scope":grant.scopes})).into_response()
}
fn local(app: &AppState, urls: &Urls, headers: &HeaderMap) -> bool {
    Url::parse(&urls.issuer).is_ok_and(|u| loopback(&u))
        && headers
            .get("x-wonder-loopback-capability")
            .and_then(|v| v.to_str().ok())
            == Some(app.loopback_capability.as_str())
}
pub(super) async fn pairing_code(
    State(app): State<AppState>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
) -> Response {
    if !local(&app, &urls, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let bytes = secret();
    // 60 random bits, avoiding ambiguous characters when typed on another device.
    let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let random = URL_SAFE_NO_PAD.decode(bytes).unwrap();
    let code: String = random[..12]
        .iter()
        .map(|b| alphabet[(*b & 31) as usize] as char)
        .collect();
    let expires = now() + PAIRING_MS;
    match app.store.mcp_pairing_code(&hash(&code), expires).await {
        Ok(()) => Json(
            json!({"pairingCode":format!("{}-{}",&code[..6],&code[6..]),"expiresAtMs":expires}),
        )
        .into_response(),
        Err(_) => unavailable(),
    }
}
pub(super) async fn clients(
    State(app): State<AppState>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
) -> Response {
    if !local(&app, &urls, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match (app.store.mcp_clients().await, app.store.mcp_grants().await) {
        (Ok(clients), Ok(grants)) => {
            Json(json!({"clients":clients,"grants":grants})).into_response()
        }
        _ => unavailable(),
    }
}
pub(super) async fn revoke(
    State(app): State<AppState>,
    Extension(urls): Extension<Urls>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !local(&app, &urls, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match app.store.mcp_revoke_client(&id, now()).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}
