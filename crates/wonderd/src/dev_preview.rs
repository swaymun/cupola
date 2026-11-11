//! Browser preview: a Project's local dev servers, found on this Mac and
//! proxied to the paired phone under the phone's existing session.
//!
//! Detection lists TCP listeners that this user's processes opened while
//! working in the thread's folder (not in another checkout nested inside it),
//! reachable on loopback, and that serve a web page at `/`. The proxy forwards only to such a port on 127.0.0.1 or
//! [::1]; it never takes a host from the request, follows no redirects, keeps
//! the phone's Wonder session cookie and CSRF token away from the dev server,
//! bounds bodies, and passes WebSocket upgrades (hot reload) through.
use crate::{projects, AppState, AuthenticatedDevice, OwnerAuthority};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path as AxumPath, Query, Request, State,
    },
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A scan is reused this long; the phone polls while a conversation is open.
const SCAN_TTL: Duration = Duration::from_secs(2);
/// A scan that takes longer than this answers with nothing rather than hang.
const SCAN_DEADLINE: Duration = Duration::from_secs(2);
/// Page check and title for a listener, reused this long: a server that stops
/// serving pages leaves the list within this time.
const PROBE_TTL: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const PROBE_BYTES: usize = 64 * 1024;
/// A Project with more listeners than this shows the lowest ports.
const MAX_SERVERS: usize = 12;
/// Uploads a dev page sends (form posts, JSON); larger ones are refused.
pub(crate) const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;
/// One proxied response; dev bundles are a few MB, source maps more.
pub(crate) const MAX_RESPONSE_BODY: u64 = 64 * 1024 * 1024;
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
/// A streamed response (server-sent events) may idle this long between chunks.
const RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SOCKET_MESSAGE: usize = 16 * 1024 * 1024;
/// Redirects within the dev server reach the phone under this header, as a
/// path on the preview's own origin; the phone's HTTP stack never follows them.
pub(crate) const LOCATION_HEADER: &str = "x-wonder-preview-location";
/// A redirect to another site, which the phone may open as a normal page.
pub(crate) const EXTERNAL_LOCATION_HEADER: &str = "x-wonder-preview-external-location";
/// The dev page's own cookies travel under these names so they never mix with
/// the Wonder session cookie on the same connection.
pub(crate) const COOKIE_HEADER: &str = "x-wonder-preview-cookie";
pub(crate) const SET_COOKIE_HEADER: &str = "x-wonder-preview-set-cookie";
/// The dev server's own security policy, renamed so Wonder's API policy on the
/// response cannot be confused with the page's.
pub(crate) const CSP_HEADER: &str = "x-wonder-preview-content-security-policy";
/// Marks a response the dev server gave, as opposed to Wonder's own refusal
/// (a stopped server, an ended session), which the phone explains natively.
pub(crate) const UPSTREAM_HEADER: &str = "x-wonder-preview-upstream";

/// Request headers a page may send on to its dev server.
const FORWARDED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "cache-control",
    "content-type",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "pragma",
    "range",
    "user-agent",
    "x-requested-with",
];
/// Response headers the phone needs from the dev server.
const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
    "accept-ranges",
    "content-disposition",
    "content-language",
    "content-range",
    "content-type",
    "etag",
    "last-modified",
    "vary",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listener {
    pub port: u16,
    pub pid: i32,
    pub process_name: String,
    /// The loopback address that reaches the listener.
    pub address: IpAddr,
}

#[derive(Clone, Debug)]
struct Probe {
    page: bool,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevServer {
    pub port: u16,
    pub process_name: String,
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevServerList {
    pub servers: Vec<DevServer>,
}

type ScanCache = HashMap<String, (Instant, Arc<Vec<Listener>>)>;
static SCANS: LazyLock<Mutex<ScanCache>> = LazyLock::new(Default::default);
type ProbeCache = HashMap<(i32, u16), (Instant, Probe)>;
static PROBES: LazyLock<Mutex<ProbeCache>> = LazyLock::new(Default::default);
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .http1_only()
        .build()
        .expect("preview HTTP client")
});

fn error(status: StatusCode, detail: &str) -> Response {
    (status, detail.to_owned()).into_response()
}

/// Where a list or proxy request looks for dev servers: one thread's folder
/// (without the Project's checkouts nested inside it), or every Project
/// folder when no thread is named.
#[derive(Clone, Debug)]
struct Scope {
    key: String,
    roots: Vec<PathBuf>,
    nested: Vec<PathBuf>,
}

/// The scope a request names, checked as every Project and thread route
/// checks folders: the thread must belong to this Project, and its folder must
/// be one of the Project's (a saved folder or a linked worktree of one).
async fn scope(
    state: &AppState,
    project_id: &str,
    conversation_id: Option<&str>,
) -> Result<Scope, (StatusCode, &'static str)> {
    let unavailable = (
        StatusCode::SERVICE_UNAVAILABLE,
        "Projects are unavailable right now.",
    );
    let project = state
        .store
        .project(project_id)
        .await
        .map_err(|_| unavailable)?
        .ok_or((StatusCode::NOT_FOUND, "This project was removed."))?;
    let conversation = match conversation_id.filter(|id| !id.is_empty()) {
        Some(id) => Some(
            state
                .store
                .project_conversation(id)
                .await
                .map_err(|_| unavailable)?
                .filter(|conversation| conversation.project_id == project.id)
                .ok_or((StatusCode::NOT_FOUND, "This chat is not in this project."))?,
        ),
        None => None,
    };
    let Some(conversation) = conversation.filter(|c| !c.cwd.is_empty()) else {
        projects::validate_execution_roots(&project, &state.denied_roots)
            .map_err(|message| (StatusCode::FORBIDDEN, message))?;
        return Ok(Scope {
            key: project_id.to_owned(),
            roots: project
                .roots
                .iter()
                .map(|root| PathBuf::from(&root.canonical_path))
                .collect(),
            nested: Vec::new(),
        });
    };
    let checkouts = projects::checkout_paths(state, &project).await;
    let project = projects::thread_project(state, project, &conversation.cwd).await;
    projects::validate_execution_roots(&project, &state.denied_roots)
        .map_err(|message| (StatusCode::FORBIDDEN, message))?;
    let root = project.root_for(&conversation.cwd).ok_or((
        StatusCode::FORBIDDEN,
        "This chat's folder was removed from its project.",
    ))?;
    Ok(Scope {
        key: format!("{project_id}\n{}", conversation.conversation_id),
        roots: vec![PathBuf::from(&root.canonical_path)],
        nested: checkouts.into_iter().map(PathBuf::from).collect(),
    })
}

/// Listeners in a scope, from a scan no older than `SCAN_TTL`.
async fn listeners(scope: Scope) -> Arc<Vec<Listener>> {
    if let Some((at, found)) = SCANS.lock().unwrap().get(&scope.key) {
        if at.elapsed() < SCAN_TTL {
            return found.clone();
        }
    }
    let key = scope.key.clone();
    let scan = tokio::task::spawn_blocking(move || {
        // Discovered worktree paths may name a folder through a symlink.
        let nested = nested_checkouts(&scope.roots, &scope.nested);
        scan_listeners(&scope.roots, &nested, Instant::now() + SCAN_DEADLINE)
    });
    let found = Arc::new(
        tokio::time::timeout(SCAN_DEADLINE + Duration::from_millis(500), scan)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
    );
    let mut scans = SCANS.lock().unwrap();
    scans.retain(|_, (at, _)| at.elapsed() < SCAN_TTL * 30);
    scans.insert(key, (Instant::now(), found.clone()));
    found
}

/// The checkouts strictly inside one of `roots`, canonical.
fn nested_checkouts(roots: &[PathBuf], checkouts: &[PathBuf]) -> Vec<PathBuf> {
    checkouts
        .iter()
        .filter_map(|path| crate::canonical_path::canonicalize(path).ok())
        .filter(|path| {
            roots
                .iter()
                .any(|root| path != root && path.starts_with(root))
        })
        .collect()
}

#[derive(serde::Deserialize)]
pub(crate) struct ServersQuery {
    conversation: Option<String>,
    /// A link's port (`http://localhost:8000/docs` in an agent's reply): the
    /// answer lists that one server when it runs for this thread, even when
    /// `/` is not a page, so the link opens in the preview at its own path.
    port: Option<u16>,
}

/// The thread a proxied request belongs to; the page's own query is the dev
/// server's, so the phone names the thread in this header.
pub(crate) const CONVERSATION_HEADER: &str = "x-wonder-preview-conversation";

pub(crate) async fn servers(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(project_id): AxumPath<String>,
    Query(query): Query<ServersQuery>,
) -> Response {
    let scope = match scope(&state, &project_id, query.conversation.as_deref()).await {
        Ok(scope) => scope,
        Err((status, detail)) => return error(status, detail),
    };
    let found = listeners(scope).await;
    let probes = futures_util::future::join_all(
        candidates(&found, query.port)
            .into_iter()
            .map(|listener| async move { (listener, probe(listener).await) }),
    )
    .await;
    let servers = probes
        .into_iter()
        .filter(|(_, probe)| probe.page || query.port.is_some())
        .map(|(listener, probe)| DevServer {
            port: listener.port,
            process_name: listener.process_name.clone(),
            title: probe.title,
        })
        .collect();
    Json(DevServerList { servers }).into_response()
}

/// The listeners a list answer considers: the lowest ports for the picker,
/// or only the one a link names, wherever it sorts. The proxy forwards to any
/// listener in the thread's folder, so a link may name one the picker hides.
pub(crate) fn candidates(found: &[Listener], port: Option<u16>) -> Vec<&Listener> {
    match port {
        Some(port) => found
            .iter()
            .find(|listener| listener.port == port)
            .into_iter()
            .collect(),
        None => found.iter().take(MAX_SERVERS).collect(),
    }
}

async fn probe(listener: &Listener) -> Probe {
    let key = (listener.pid, listener.port);
    if let Some((at, probe)) = PROBES.lock().unwrap().get(&key) {
        if at.elapsed() < PROBE_TTL {
            return probe.clone();
        }
    }
    let result = tokio::time::timeout(PROBE_TIMEOUT, read_page(listener)).await;
    let probe = match result {
        Ok(Some(title)) => Probe { page: true, title },
        _ => Probe {
            page: false,
            title: None,
        },
    };
    let mut probes = PROBES.lock().unwrap();
    probes.retain(|_, (at, _)| at.elapsed() < PROBE_TTL);
    probes.insert(key, (Instant::now(), probe.clone()));
    probe
}

/// Whether `GET /` answered with a web page a person can open: an HTML page,
/// or a redirect to another page on the same server (a sign-in page, `/app`).
/// Debugger and inspector ports (node --inspect, workerd), JSON APIs, errors
/// and redirects to other sites are not pages.
pub(crate) fn serves_page(
    status: u16,
    content_type: Option<&str>,
    location: Option<&str>,
    port: u16,
) -> bool {
    match status {
        200..=299 => content_type.is_some_and(|value| {
            let value = value.trim().to_ascii_lowercase();
            value.starts_with("text/html") || value.starts_with("application/xhtml+xml")
        }),
        300..=399 => location.is_some_and(|location| rewrite_location(location, "/", port).is_ok()),
        _ => false,
    }
}

/// `Some(title)` when the listener serves a web page at `/`.
async fn read_page(listener: &Listener) -> Option<Option<String>> {
    let url = format!(
        "http://{}/",
        SocketAddr::new(listener.address, listener.port)
    );
    let mut response = CLIENT
        .get(url)
        .header(header::HOST, format!("localhost:{}", listener.port))
        .header(header::ACCEPT, "text/html")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .ok()?;
    let text = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let (content_type, location) = (text(header::CONTENT_TYPE), text(header::LOCATION));
    if !serves_page(
        response.status().as_u16(),
        content_type.as_deref(),
        location.as_deref(),
        listener.port,
    ) {
        return None;
    }
    if !response.status().is_success() {
        return Some(None);
    }
    let mut body = Vec::new();
    while body.len() < PROBE_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    body.truncate(PROBE_BYTES);
    Some(page_title(&String::from_utf8_lossy(&body)))
}

/// The page's `<title>`, decoded and shortened for a picker row.
pub(crate) fn page_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let raw = &html[start..end];
    let decoded = raw
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    let title = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return None;
    }
    Some(title.chars().take(80).collect())
}

/// Whether a process working in `cwd` belongs to the scope: inside one of
/// `roots` and not inside a checkout nested there (another thread's
/// worktree). Matching is by path components, so `/work/app-old` is not
/// inside `/work/app`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn in_scope(cwd: &Path, roots: &[PathBuf], nested: &[PathBuf]) -> bool {
    roots.iter().any(|root| cwd.starts_with(root))
        && !nested.iter().any(|checkout| cwd.starts_with(checkout))
}

/// The loopback address that reaches a listener bound to `local`, or `None`
/// when it accepts connections only on another interface.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn loopback_target(local: IpAddr) -> Option<IpAddr> {
    match local {
        IpAddr::V4(address) if address.is_loopback() || address.is_unspecified() => {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        }
        IpAddr::V6(address) if address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()) => {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        }
        IpAddr::V6(address) if address.is_loopback() || address.is_unspecified() => {
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        }
        _ => None,
    }
}

/// Keeps one listener per port, preferring IPv4 (dual listeners are common),
/// sorted by port.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn dedupe(mut found: Vec<Listener>) -> Vec<Listener> {
    found.sort_by_key(|listener| (listener.port, listener.address.is_ipv6()));
    found.dedup_by_key(|listener| listener.port);
    found
}

/// Wonder's own listeners are never a dev server. Tests run their fixture
/// servers inside the test process, so they keep them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const EXCLUDE_SELF: bool = !cfg!(test);

#[cfg(target_os = "macos")]
fn scan_listeners(roots: &[PathBuf], nested: &[PathBuf], deadline: Instant) -> Vec<Listener> {
    macos::scan(roots, nested, deadline)
}

#[cfg(not(target_os = "macos"))]
fn scan_listeners(_roots: &[PathBuf], _nested: &[PathBuf], _deadline: Instant) -> Vec<Listener> {
    Vec::new()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::CStr;
    use std::os::raw::{c_char, c_int, c_void};

    const PROC_PIDFDSOCKETINFO: c_int = 3;
    /// `sizeof(struct socket_fdinfo)` from `<sys/proc_info.h>`; libc has no binding.
    const SOCKET_FDINFO_SIZE: usize = 792;
    // Offsets into `struct socket_fdinfo`: `proc_fileinfo` (24 bytes) then
    // `socket_info`, whose protocol union starts at 240.
    const SOI_KIND: usize = 24 + 232;
    const SOI_PROTO: usize = 24 + 240;
    const INSI_LPORT: usize = SOI_PROTO + 4;
    const INSI_VFLAG: usize = SOI_PROTO + 24;
    const INSI_LADDR: usize = SOI_PROTO + 48;
    const TCPSI_STATE: usize = SOI_PROTO + 80;
    const SOCKINFO_TCP: i32 = 2;
    const TSI_S_LISTEN: i32 = 1;
    const INI_IPV4: u8 = 0x1;
    const INI_IPV6: u8 = 0x2;
    const MAX_FDS: usize = 16 * 1024;

    pub(super) fn scan(roots: &[PathBuf], nested: &[PathBuf], deadline: Instant) -> Vec<Listener> {
        let uid = unsafe { libc::getuid() };
        let me = std::process::id() as i32;
        let mut found = Vec::new();
        for pid in all_pids() {
            if Instant::now() > deadline {
                break;
            }
            if pid <= 0 || (EXCLUDE_SELF && pid == me) {
                continue;
            }
            let Some((process_uid, name)) = bsd_info(pid) else {
                continue;
            };
            if process_uid != uid {
                continue;
            }
            let Some(cwd) = cwd(pid) else { continue };
            if !in_scope(&cwd, roots, nested) {
                continue;
            }
            for (local, port) in tcp_listeners(pid) {
                if let Some(address) = loopback_target(local) {
                    found.push(Listener {
                        port,
                        pid,
                        process_name: name.clone(),
                        address,
                    });
                }
            }
        }
        dedupe(found)
    }

    fn all_pids() -> Vec<i32> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as c_int;
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if filled <= 0 {
            return Vec::new();
        }
        pids.truncate((filled as usize).min(pids.len()));
        pids
    }

    fn bsd_info(pid: i32) -> Option<(libc::uid_t, String)> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let name = c_name(&info.pbi_name).or_else(|| c_name(&info.pbi_comm))?;
        Some((info.pbi_uid, name))
    }

    fn c_name(bytes: &[c_char]) -> Option<String> {
        let bytes: Vec<u8> = bytes
            .iter()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        let name = String::from_utf8_lossy(&bytes).trim().to_owned();
        (!name.is_empty()).then_some(name)
    }

    fn cwd(pid: i32) -> Option<PathBuf> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        let path = path.to_str().ok()?;
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    fn tcp_listeners(pid: i32) -> Vec<(IpAddr, u16)> {
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        let capacity = (needed as usize / entry + 32).min(MAX_FDS);
        let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; capacity];
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (capacity * entry) as c_int,
            )
        };
        if read <= 0 {
            return Vec::new();
        }
        fds.truncate(read as usize / entry);
        let mut listeners = Vec::new();
        // u64 storage keeps the kernel's 8-byte aligned struct aligned.
        let mut buffer = [0u64; SOCKET_FDINFO_SIZE / 8];
        for fd in fds
            .iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
        {
            buffer.fill(0);
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDSOCKETINFO,
                    buffer.as_mut_ptr() as *mut c_void,
                    SOCKET_FDINFO_SIZE as c_int,
                )
            };
            if read as usize != SOCKET_FDINFO_SIZE {
                continue;
            }
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), SOCKET_FDINFO_SIZE) };
            if let Some(listener) = parse_socket(bytes) {
                listeners.push(listener);
            }
        }
        listeners
    }

    fn i32_at(bytes: &[u8], offset: usize) -> i32 {
        i32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    /// A listening TCP socket's local address and port.
    pub(super) fn parse_socket(bytes: &[u8]) -> Option<(IpAddr, u16)> {
        if bytes.len() < SOCKET_FDINFO_SIZE
            || i32_at(bytes, SOI_KIND) != SOCKINFO_TCP
            || i32_at(bytes, TCPSI_STATE) != TSI_S_LISTEN
        {
            return None;
        }
        // `insi_lport` holds the port in network byte order.
        let port = u16::from_be(i32_at(bytes, INSI_LPORT) as u16);
        let flags = bytes[INSI_VFLAG];
        let address = &bytes[INSI_LADDR..INSI_LADDR + 16];
        let local = if flags & INI_IPV4 != 0 {
            IpAddr::V4(Ipv4Addr::new(
                address[12],
                address[13],
                address[14],
                address[15],
            ))
        } else if flags & INI_IPV6 != 0 {
            IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(address).ok()?))
        } else {
            return None;
        };
        (port != 0).then_some((local, port))
    }
}

/// Path after `/api/v1/projects/{id}/preview/{port}` in the request, still
/// percent-encoded, with its query. `None` for a path that could leave the
/// dev server's root.
pub(crate) fn upstream_path(raw_path: &str, query: Option<&str>, port: u16) -> Option<String> {
    let mut segments = raw_path.splitn(7, '/');
    let prefix: Vec<&str> = segments.by_ref().take(6).collect();
    if prefix.len() != 6 || prefix[..4] != ["", "api", "v1", "projects"] || prefix[5] != "preview" {
        return None;
    }
    let rest = segments.next()?;
    let rest = match rest.split_once('/') {
        Some((given, rest)) if given == port.to_string() => format!("/{rest}"),
        None if rest == port.to_string() => "/".to_owned(),
        _ => return None,
    };
    let rest = rest.as_str();
    if rest.contains('\\') || rest.contains('\0') {
        return None;
    }
    let decoded = percent_decode(rest)?;
    if decoded.contains('\\')
        || decoded.contains('\0')
        || decoded
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
    {
        return None;
    }
    // Encoded slashes would hide a `..` from the check above.
    if rest.to_ascii_lowercase().contains("%2f") || rest.to_ascii_lowercase().contains("%5c") {
        return None;
    }
    Some(match query {
        Some(query) if !query.is_empty() => format!("{rest}?{query}"),
        _ => rest.to_owned(),
    })
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Rewrites a dev server's `Location` relative to the request it answered:
/// `Ok(path)` stays on the dev server, `Err(Some(url))` leaves it for another
/// web site, `Err(None)` is unusable.
pub(crate) fn rewrite_location(
    location: &str,
    request_path: &str,
    port: u16,
) -> Result<String, Option<String>> {
    let base =
        reqwest::Url::parse(&format!("http://localhost:{port}{request_path}")).map_err(|_| None)?;
    let target = base.join(location).map_err(|_| None)?;
    let same_server = matches!(target.scheme(), "http" | "https")
        && matches!(
            target.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0")
        )
        && target.port_or_known_default() == Some(port);
    if same_server {
        let mut path = target.path().to_owned();
        if let Some(query) = target.query() {
            path.push('?');
            path.push_str(query);
        }
        if let Some(fragment) = target.fragment() {
            path.push('#');
            path.push_str(fragment);
        }
        return Ok(path);
    }
    if matches!(target.scheme(), "http" | "https") {
        Err(Some(target.to_string()))
    } else {
        Err(None)
    }
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// `/api/v1/projects/{project_id}/preview/{port}` and everything below it.
pub(crate) async fn proxy(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    AxumPath(params): AxumPath<HashMap<String, String>>,
    request: Request,
) -> Response {
    let (Some(project_id), Some(port)) = (
        params.get("project_id"),
        params.get("port").and_then(|port| port.parse::<u16>().ok()),
    ) else {
        return error(StatusCode::NOT_FOUND, "This preview is unavailable.");
    };
    if port == 0 {
        return error(StatusCode::NOT_FOUND, "This preview is unavailable.");
    }
    let conversation = request
        .headers()
        .get(CONVERSATION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let scope = match scope(&state, project_id, conversation.as_deref()).await {
        Ok(scope) => scope,
        Err((status, detail)) => return error(status, detail),
    };
    let Some(path) = upstream_path(request.uri().path(), request.uri().query(), port) else {
        return error(StatusCode::BAD_REQUEST, "This page address is not allowed.");
    };
    let found = listeners(scope).await;
    let Some(listener) = found.iter().find(|listener| listener.port == port).cloned() else {
        return error(
            StatusCode::NOT_FOUND,
            "This dev server is no longer running for this chat.",
        );
    };
    if is_websocket(request.headers()) {
        let device = request.extensions().get::<AuthenticatedDevice>().cloned();
        return proxy_socket(state, listener, path, request, device).await;
    }
    proxy_http(listener, path, request).await
}

async fn proxy_http(listener: Listener, path: String, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let method = match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
        Ok(method) if parts.method != Method::CONNECT && parts.method != Method::TRACE => method,
        _ => {
            return error(
                StatusCode::METHOD_NOT_ALLOWED,
                "This request is not allowed.",
            )
        }
    };
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(body) => body,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This upload is larger than the preview allows (16 MB).",
            )
        }
    };
    let url = format!(
        "http://{}{path}",
        SocketAddr::new(listener.address, listener.port)
    );
    let Ok(url) = reqwest::Url::parse(&url) else {
        return error(StatusCode::BAD_REQUEST, "This page address is not allowed.");
    };
    if url.port() != Some(listener.port) || url.host_str() != Some(&host_literal(listener.address))
    {
        return error(StatusCode::BAD_REQUEST, "This page address is not allowed.");
    }
    let mut upstream = CLIENT
        .request(method.clone(), url)
        .headers(forward_request_headers(
            &parts.headers,
            listener.port,
            &method,
        ));
    if !body.is_empty() {
        upstream = upstream.body(body);
    }
    let response = match tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, upstream.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "The dev server did not answer. Check that it is still running.",
            )
        }
        Err(_) => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "The dev server took too long to answer.",
            )
        }
    };
    let request_path = path.split(['?', '#']).next().unwrap_or("/").to_owned();
    respond(response, &request_path, listener.port)
}

fn host_literal(address: IpAddr) -> String {
    match address {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => format!("[{address}]"),
    }
}

/// Only what a page needs: content negotiation, caching validators, the
/// page's own cookies. The Wonder session, CSRF token, Origin and Referer of
/// the phone's request never reach the dev server.
pub(crate) fn forward_request_headers(
    incoming: &HeaderMap,
    port: u16,
    method: &reqwest::Method,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for name in FORWARDED_REQUEST_HEADERS {
        for value in incoming.get_all(*name) {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.append(name, value);
            }
        }
    }
    if let Some(cookie) = incoming
        .get(COOKIE_HEADER)
        .and_then(|value| reqwest::header::HeaderValue::from_bytes(value.as_bytes()).ok())
    {
        headers.insert(reqwest::header::COOKIE, cookie);
    }
    let origin = format!("http://localhost:{port}");
    if let Ok(host) = reqwest::header::HeaderValue::from_str(&format!("localhost:{port}")) {
        headers.insert(reqwest::header::HOST, host);
    }
    // A page's own form posts and fetches are same-origin on the dev server.
    if !matches!(*method, reqwest::Method::GET | reqwest::Method::HEAD) {
        if let Ok(origin) = reqwest::header::HeaderValue::from_str(&origin) {
            headers.insert(reqwest::header::ORIGIN, origin);
        }
    }
    // The phone gets bytes as stored; Wonder adds no decoder of its own.
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    headers
}

fn respond(response: reqwest::Response, request_path: &str, port: u16) -> Response {
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for name in FORWARDED_RESPONSE_HEADERS {
        for value in response.headers().get_all(*name) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.append(name, value);
            }
        }
    }
    for value in response.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            headers.append(HeaderName::from_static(SET_COOKIE_HEADER), value);
        }
    }
    if let Some(value) = response
        .headers()
        .get(reqwest::header::CONTENT_SECURITY_POLICY)
        .and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok())
    {
        headers.insert(HeaderName::from_static(CSP_HEADER), value);
    }
    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BODY {
            return error(
                StatusCode::BAD_GATEWAY,
                "This file is larger than the preview allows (64 MB).",
            );
        }
    }
    if status.is_redirection() {
        if let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        {
            match rewrite_location(location, request_path, port) {
                Ok(path) => {
                    if let Ok(value) = HeaderValue::from_str(&path) {
                        headers.insert(HeaderName::from_static(LOCATION_HEADER), value);
                    }
                }
                Err(Some(external)) => {
                    let mut refused = error(
                        StatusCode::BAD_GATEWAY,
                        "The dev server sent this page to another web site.",
                    );
                    if let Ok(value) = HeaderValue::from_str(&external) {
                        refused
                            .headers_mut()
                            .insert(HeaderName::from_static(EXTERNAL_LOCATION_HEADER), value);
                    }
                    return refused;
                }
                Err(None) => {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "The dev server sent an address the preview cannot open.",
                    )
                }
            }
        }
    }
    let stream = futures_util::stream::unfold(
        (response, 0u64, false),
        |(mut response, sent, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(RESPONSE_IDLE_TIMEOUT, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    let sent = sent + chunk.len() as u64;
                    if sent > MAX_RESPONSE_BODY {
                        return Some((
                            Err(std::io::Error::other("preview response exceeded its limit")),
                            (response, sent, true),
                        ));
                    }
                    Some((Ok::<Bytes, std::io::Error>(chunk), (response, sent, false)))
                }
                Ok(Ok(None)) => None,
                Ok(Err(error)) => Some((Err(std::io::Error::other(error)), (response, sent, true))),
                Err(_) => Some((
                    Err(std::io::Error::other("preview response stalled")),
                    (response, sent, true),
                )),
            }
        },
    );
    headers.insert(
        HeaderName::from_static(UPSTREAM_HEADER),
        HeaderValue::from_static("1"),
    );
    let mut reply = Response::new(Body::from_stream(stream));
    *reply.status_mut() = status;
    *reply.headers_mut() = headers;
    reply
}

async fn proxy_socket(
    state: AppState,
    listener: Listener,
    path: String,
    request: Request,
    device: Option<AuthenticatedDevice>,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let protocols: Vec<String> = parts
        .headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect();
    let url = format!(
        "ws://{}{path}",
        SocketAddr::new(listener.address, listener.port)
    );
    let Ok(mut upstream_request) =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
    else {
        return error(StatusCode::BAD_REQUEST, "This page address is not allowed.");
    };
    let headers = upstream_request.headers_mut();
    if let Ok(host) = HeaderValue::from_str(&format!("localhost:{}", listener.port)) {
        headers.insert(header::HOST, host);
    }
    if let Ok(origin) = HeaderValue::from_str(&format!("http://localhost:{}", listener.port)) {
        headers.insert(header::ORIGIN, origin);
    }
    if !protocols.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&protocols.join(", ")) {
            headers.insert(header::SEC_WEBSOCKET_PROTOCOL, value);
        }
    }
    if let Some(cookie) = parts.headers.get(COOKIE_HEADER).cloned() {
        headers.insert(header::COOKIE, cookie);
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_SOCKET_MESSAGE))
        .max_frame_size(Some(MAX_SOCKET_MESSAGE));
    let connect =
        tokio_tungstenite::connect_async_with_config(upstream_request, Some(config), false);
    let (upstream, handshake) = match tokio::time::timeout(Duration::from_secs(5), connect).await {
        Ok(Ok(connected)) => connected,
        _ => {
            return error(
                StatusCode::BAD_GATEWAY,
                "The dev server did not accept the live connection.",
            )
        }
    };
    let chosen = handshake
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut upgrade = upgrade.max_message_size(MAX_SOCKET_MESSAGE);
    if let Some(chosen) = chosen {
        upgrade = upgrade.protocols([chosen]);
    }
    let revocations = state.revocations.subscribe();
    upgrade.on_upgrade(move |socket| pump(socket, upstream, revocations, device))
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn pump(
    socket: WebSocket,
    upstream: Upstream,
    mut revocations: tokio::sync::broadcast::Receiver<String>,
    device: Option<AuthenticatedDevice>,
) {
    use tokio_tungstenite::tungstenite::Message as Upstreamed;
    let (mut phone_tx, mut phone_rx) = socket.split();
    let (mut dev_tx, mut dev_rx) = upstream.split();
    loop {
        tokio::select! {
            message = phone_rx.next() => {
                let forwarded = match message {
                    Some(Ok(Message::Text(text))) => Upstreamed::text(text.as_str()),
                    Some(Ok(Message::Binary(bytes))) => Upstreamed::binary(bytes),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                if dev_tx.send(forwarded).await.is_err() { break; }
            }
            message = dev_rx.next() => {
                let forwarded = match message {
                    Some(Ok(Upstreamed::Text(text))) => Message::Text(text.as_str().into()),
                    Some(Ok(Upstreamed::Binary(bytes))) => Message::Binary(bytes),
                    Some(Ok(Upstreamed::Ping(_) | Upstreamed::Pong(_) | Upstreamed::Frame(_))) => continue,
                    _ => break,
                };
                if phone_tx.send(forwarded).await.is_err() { break; }
            }
            revoked = revocations.recv() => {
                let ended = match revoked {
                    Ok(id) => device.as_ref().is_some_and(|device| device.device_id == id),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => false,
                    Err(_) => true,
                };
                if ended { break; }
            }
        }
    }
    let _ = phone_tx
        .send(Message::Close(Some(CloseFrame {
            code: 1001,
            reason: "Preview closed".into(),
        })))
        .await;
    let _ = dev_tx.close().await;
}

#[cfg(all(test, unix))]
mod tests;
