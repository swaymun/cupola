//! The home network route: a TLS-only listener on each private address of this
//! computer, so a phone on the same network reaches the daemon directly.
//!
//! The certificate is self-signed by the host key. A phone trusts it by the pin it
//! saved when it paired (base64url SHA-256 of the SPKI), not by a CA. Loopback
//! stays the daemon's main listener; this module never binds a wildcard or a
//! public address. A request that arrives here carries [`HomeRoute`], so the auth
//! middleware trusts exactly this listener's origin, and the pairing-link reply,
//! MCP and loopback-only routes are refused before the router sees them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use wonder_api::pairing_protocol::HostKey;

use crate::logging::JsonlLogger;

pub const DEFAULT_PORT: u16 = 3778;
/// Interface changes are picked up by polling; the plan accepts 30 s for a first version.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Open connections per home address. A phone needs a handful; anyone else on
/// the network can open sockets and hold them in the TLS handshake.
pub(crate) const MAX_CONNECTIONS: usize = 128;
const HOST_KEY_FILE: &str = "host-key";

/// Interface name prefixes of bridges, tunnels and VPNs. A container bridge holds
/// a private address, so a private range alone does not make a home network.
const VIRTUAL_PREFIXES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "lxdbr",
    "virbr",
    "vEthernet",
    "utun",
    "tailscale",
    "wg",
    "zt",
    "bridge",
    "awdl",
    "llw",
];

/// Why an address is or is not a home network route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Advertise,
    LoopbackOrLinkLocal,
    /// `100.64.0.0/10` and `fd7a:115c:a1e0::/48`: the Tailscale route owns these.
    TailscaleRange,
    /// Never bind a public address.
    Public,
    InterfaceDown,
    VirtualInterface,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterfaceAddress {
    pub name: String,
    pub up: bool,
    pub ip: IpAddr,
}

/// `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, the ranges the Tailscale route owns.
pub fn tailscale_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

pub fn classify(name: &str, up: bool, ip: IpAddr) -> Verdict {
    let (loopback_or_link_local, private) = match ip {
        IpAddr::V4(v4) => (
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
            v4.is_private(),
        ),
        IpAddr::V6(v6) => (
            v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unspecified(),
            v6.segments()[0] & 0xfe00 == 0xfc00,
        ),
    };
    if loopback_or_link_local {
        Verdict::LoopbackOrLinkLocal
    } else if tailscale_address(ip) {
        Verdict::TailscaleRange
    } else if !private {
        Verdict::Public
    } else if !up {
        Verdict::InterfaceDown
    } else if VIRTUAL_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        Verdict::VirtualInterface
    } else {
        Verdict::Advertise
    }
}

/// The addresses to listen on, sorted and without duplicates.
pub fn home_addresses(interfaces: &[InterfaceAddress]) -> Vec<IpAddr> {
    let mut addresses: Vec<IpAddr> = interfaces
        .iter()
        .filter(|entry| classify(&entry.name, entry.up, entry.ip) == Verdict::Advertise)
        .map(|entry| entry.ip)
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

pub fn origin(ip: IpAddr, port: u16) -> String {
    format!("https://{}", SocketAddr::new(ip, port))
}

/// This computer's interface addresses. An interface counts as up when it is
/// administratively up and has carrier.
#[cfg(unix)]
pub fn interface_addresses() -> Vec<InterfaceAddress> {
    let mut entries = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below; each node and its
    // address stay valid until freeifaddrs.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return entries;
        }
        let mut node = head;
        while !node.is_null() {
            let entry = &*node;
            node = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
                continue;
            }
            let ip = match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(u32::from_be(address.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 => {
                    let address = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::from(address.sin6_addr.s6_addr)
                }
                _ => continue,
            };
            let flags = entry.ifa_flags as libc::c_int;
            entries.push(InterfaceAddress {
                name: std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_string_lossy()
                    .into_owned(),
                up: flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0,
                ip,
            });
        }
        libc::freeifaddrs(head);
    }
    entries
}

#[cfg(windows)]
pub fn interface_addresses() -> Vec<InterfaceAddress> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

    // Windows recommends starting with 15 KiB. Allocate typed elements so the
    // buffer has the native adapter structure's alignment, including on ARM64.
    let mut bytes = 15 * 1024u32;
    for _ in 0..3 {
        if bytes > 16 * 1024 * 1024 {
            return Vec::new();
        }
        let count = (bytes as usize).div_ceil(std::mem::size_of::<IP_ADAPTER_ADDRESSES_LH>());
        let mut buffer = vec![IP_ADAPTER_ADDRESSES_LH::default(); count];
        // SAFETY: buffer is writable, aligned and at least `bytes` long. All
        // returned pointers remain valid until this iteration drops it.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null(),
                buffer.as_mut_ptr(),
                &mut bytes,
            )
        };
        match result {
            NO_ERROR => return unsafe { windows_adapter_addresses(buffer.as_ptr()) },
            // Adapter changes may require a larger buffer between calls.
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// Copies the native list while its owning GetAdaptersAddresses buffer lives.
/// Every non-null pointer in `head` must refer to a valid native list member.
#[cfg(windows)]
unsafe fn windows_adapter_addresses(
    mut head: *const windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_ADDRESSES_LH,
) -> Vec<InterfaceAddress> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};

    let mut entries = Vec::new();
    while let Some(adapter) = head.as_ref() {
        head = adapter.Next;
        if matches!(adapter.IfType, IF_TYPE_SOFTWARE_LOOPBACK | IF_TYPE_TUNNEL) {
            continue;
        }
        let name = if !adapter.FriendlyName.is_null() {
            let length = (0..256)
                .take_while(|&i| *adapter.FriendlyName.add(i) != 0)
                .count();
            String::from_utf16_lossy(std::slice::from_raw_parts(adapter.FriendlyName, length))
        } else if !adapter.AdapterName.is_null() {
            std::ffi::CStr::from_ptr(adapter.AdapterName.cast())
                .to_string_lossy()
                .into_owned()
        } else {
            continue;
        };
        let mut node = adapter.FirstUnicastAddress;
        while let Some(address) = node.as_ref() {
            node = address.Next;
            let socket = address.Address;
            if socket.lpSockaddr.is_null() || socket.iSockaddrLength < 2 {
                continue;
            }
            let ip = match (*socket.lpSockaddr).sa_family {
                AF_INET
                    if socket.iSockaddrLength as usize >= std::mem::size_of::<SOCKADDR_IN>() =>
                {
                    let v4 = &*socket.lpSockaddr.cast::<SOCKADDR_IN>();
                    IpAddr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes())
                }
                AF_INET6
                    if socket.iSockaddrLength as usize >= std::mem::size_of::<SOCKADDR_IN6>() =>
                {
                    let v6 = &*socket.lpSockaddr.cast::<SOCKADDR_IN6>();
                    IpAddr::from(v6.sin6_addr.u.Byte)
                }
                _ => continue,
            };
            entries.push(InterfaceAddress {
                name: name.clone(),
                up: adapter.OperStatus == IfOperStatusUp,
                ip,
            });
        }
    }
    entries
}

/// Reads the host key from the data directory, creating it (0600) on first use.
/// A damaged key file is an error rather than a silent new key: paired phones
/// pinned the old one.
pub fn load_or_create_host_key(data_dir: &Path) -> io::Result<HostKey> {
    let path = data_dir.join(HOST_KEY_FILE);
    let read = |bytes: Vec<u8>| {
        HostKey::from_secret_bytes(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a valid host key", path.display()),
            )
        })
    };
    match std::fs::read(&path) {
        Ok(bytes) => return read(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let key = HostKey::generate();
    // NamedTempFile is created 0600 on Unix.
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    file.write_all(&key.to_secret_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => Ok(key),
        // Another process created it first: use theirs.
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            read(std::fs::read(&path)?)
        }
        Err(error) => Err(error.error),
    }
}

/// [`load_or_create_host_key`] off the async runtime, for request handlers.
pub async fn host_key(data_dir: &Path) -> io::Result<HostKey> {
    let data_dir = data_dir.to_owned();
    tokio::task::spawn_blocking(move || load_or_create_host_key(&data_dir))
        .await
        .map_err(io::Error::other)?
}

/// PKCS#8 for a P-256 key: the private scalar and its uncompressed public point.
fn pkcs8_der(host_key: &HostKey) -> Vec<u8> {
    const PREFIX: [u8; 36] = [
        0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d,
        0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30,
        0x6b, 0x02, 0x01, 0x01, 0x04, 0x20,
    ];
    const PUBLIC_KEY_TAG: [u8; 5] = [0xa1, 0x44, 0x03, 0x42, 0x00];
    let spki = host_key.public_key_spki();
    let point = &spki[spki.len() - 65..];
    let mut der = Vec::with_capacity(PREFIX.len() + 32 + PUBLIC_KEY_TAG.len() + 65);
    der.extend_from_slice(&PREFIX);
    der.extend_from_slice(&host_key.to_secret_bytes());
    der.extend_from_slice(&PUBLIC_KEY_TAG);
    der.extend_from_slice(point);
    der
}

/// A TLS 1.2/1.3 server config whose only certificate is self-signed by the host
/// key, so its SPKI hash is the host key pin. HTTP/1.1 only, which keeps the
/// event socket's WebSocket upgrade on the path phones already use.
pub fn tls_config(host_key: &HostKey) -> Result<Arc<rustls::ServerConfig>, String> {
    let pkcs8 = PrivatePkcs8KeyDer::from(pkcs8_der(host_key));
    let key_pair =
        rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|error| format!("host key: {error}"))?;
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Wonder");
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|error| format!("certificate: {error}"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.der().clone()], PrivateKeyDer::Pkcs8(pkcs8))
        .map_err(|error| error.to_string())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Set on every request that arrived over a home network listener: the only
/// origin that request may present.
#[derive(Clone, Debug)]
pub(crate) struct HomeRoute {
    pub(crate) origin: String,
}

/// Phones use the API and the health checks over the home network. The
/// pairing-link reply, MCP and its OAuth endpoints, and loopback-only pairing
/// offers stay on Tailscale and loopback.
fn home_route_allows(path: &str) -> bool {
    if matches!(path, "/healthz" | "/readyz") {
        return true;
    }
    path.starts_with("/api/")
        && !path.starts_with("/api/v1/mcp")
        && !path.starts_with("/api/v1/pairing/offers")
        && !path.starts_with("/api/v1/pairing/pending")
}

pub(crate) async fn handle(router: Router, origin: &str, mut request: Request<Body>) -> Response {
    if !home_route_allows(request.uri().path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request
        .headers()
        .contains_key("x-wonder-loopback-capability")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(HomeRoute {
        origin: origin.to_owned(),
    });
    match router.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// A connection's stream that fails once its listener stops. Upgraded RPC
/// sockets run outside the connection task, so ending the stream is what
/// closes them with the listener.
struct Stoppable<S> {
    inner: S,
    stopped: Pin<Box<dyn Future<Output = ()> + Send>>,
    done: bool,
}

impl<S> Stoppable<S> {
    /// Stops when `stop`'s sender sends or is dropped.
    fn new(inner: S, mut stop: watch::Receiver<()>) -> Self {
        Self {
            inner,
            stopped: Box::pin(async move {
                let _ = stop.changed().await;
            }),
            done: false,
        }
    }

    fn poll_stopped(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.done && self.stopped.as_mut().poll(cx).is_ready() {
            self.done = true;
        }
        if self.done {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the home network route stopped",
            )))
        } else {
            Poll::Pending
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Stoppable<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Poll::Ready(error) = this.poll_stopped(cx) {
            return Poll::Ready(error);
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Stoppable<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Poll::Ready(Err(error)) = this.poll_stopped(cx) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Poll::Ready(error) = this.poll_stopped(cx) {
            return Poll::Ready(error);
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.poll_stopped(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

async fn serve_connection(
    stream: Stoppable<TcpStream>,
    acceptor: TlsAcceptor,
    router: Router,
    origin: Arc<str>,
) {
    // A plain HTTP request or a client that rejects the pin ends here.
    let Ok(Ok(tls)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await else {
        return;
    };
    let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
        let router = router.clone();
        let origin = origin.clone();
        async move { Ok::<_, Infallible>(handle(router, &origin, request.map(Body::new)).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(tls), service)
        .with_upgrades()
        .await;
}

/// Accepts TLS connections on one address until the task is aborted. Aborting
/// it also ends every connection it accepted, upgraded RPC sockets included.
async fn serve(listener: TcpListener, acceptor: TlsAcceptor, router: Router, origin: Arc<str>) {
    // Dropped with this future, which stops every `Stoppable` stream.
    let (_stop, stop) = watch::channel(());
    // Dropped with this future too, which aborts connections mid-request.
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => {
                // Out of descriptors or a reset before accept; try again shortly.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        if connections.len() >= MAX_CONNECTIONS {
            // Closed at once; existing connections keep working.
            drop(stream);
            continue;
        }
        connections.spawn(serve_connection(
            Stoppable::new(stream, stop.clone()),
            acceptor.clone(),
            router.clone(),
            origin.clone(),
        ));
    }
}

/// The home network origins the listeners serve right now, and the host key
/// pin they present, so host status can tell a paired phone where else to
/// reach this computer. Empty while the route is off or nothing is bound.
#[derive(Clone, Debug, Default)]
pub struct HomeRoutes(Arc<std::sync::RwLock<Serving>>);

#[derive(Clone, Debug, Default)]
struct Serving {
    origins: Vec<String>,
    host_key_pin: Option<String>,
}

impl HomeRoutes {
    /// The origins answering now, in address order.
    pub fn origins(&self) -> Vec<String> {
        self.read().origins
    }

    /// The pin of the key the listeners present, while any listener runs.
    pub fn host_key_pin(&self) -> Option<String> {
        let serving = self.read();
        serving.host_key_pin.filter(|_| !serving.origins.is_empty())
    }

    fn read(&self) -> Serving {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, origins: Vec<String>, host_key_pin: Option<String>) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Serving {
            origins,
            host_key_pin,
        };
    }

    #[cfg(test)]
    pub(crate) fn serving(origins: Vec<String>, host_key_pin: &str) -> Self {
        let routes = Self::default();
        routes.set(origins, Some(host_key_pin.to_owned()));
        routes
    }
}

struct BoundListener {
    address: SocketAddr,
    task: JoinHandle<()>,
}

/// One listener per home address. Dropping it stops them all.
struct Listeners {
    port: u16,
    published: HomeRoutes,
    host_key_pin: String,
    acceptor: TlsAcceptor,
    router: Router,
    logger: Arc<JsonlLogger>,
    running: HashMap<IpAddr, BoundListener>,
    /// The last bind error per address, so a busy port is logged once, not every scan.
    failed: HashMap<IpAddr, String>,
}

impl Listeners {
    /// Stops listeners whose address is gone and binds new addresses.
    async fn reconcile(&mut self, wanted: &[IpAddr]) {
        let port = self.port;
        let stale: Vec<IpAddr> = self
            .running
            .iter()
            .filter(|(ip, task)| !wanted.contains(ip) || task.task.is_finished())
            .map(|(ip, _)| *ip)
            .collect();
        for ip in stale {
            if let Some(task) = self.running.remove(&ip) {
                task.task.abort();
            }
            self.log("info", "home_route_stopped", ip, None);
        }
        self.failed.retain(|ip, _| wanted.contains(ip));
        for &ip in wanted {
            if self.running.contains_key(&ip) {
                continue;
            }
            let bound = match TcpListener::bind(SocketAddr::new(ip, port)).await {
                Ok(listener) => listener.local_addr().map(|address| (listener, address)),
                Err(error) => Err(error),
            };
            match bound {
                Ok((listener, address)) => {
                    self.failed.remove(&ip);
                    let task = tokio::spawn(serve(
                        listener,
                        self.acceptor.clone(),
                        self.router.clone(),
                        origin(ip, address.port()).into(),
                    ));
                    self.running.insert(ip, BoundListener { address, task });
                    self.log("info", "home_route_listening", ip, None);
                }
                Err(error) => {
                    let error = error.to_string();
                    if self.failed.get(&ip) != Some(&error) {
                        self.log("warn", "home_route_bind_failed", ip, Some(&error));
                        self.failed.insert(ip, error);
                    }
                }
            }
        }
        let mut serving = self
            .running
            .values()
            .map(|bound| bound.address)
            .collect::<Vec<_>>();
        serving.sort();
        self.published.set(
            serving
                .into_iter()
                .map(|address| origin(address.ip(), address.port()))
                .collect(),
            Some(self.host_key_pin.clone()),
        );
    }

    fn log(&self, severity: &str, event: &str, ip: IpAddr, error: Option<&str>) {
        let _ = self.logger.record(
            severity,
            event,
            serde_json::json!({
                "address": self.running.get(&ip).map_or(SocketAddr::new(ip, self.port), |bound| bound.address),
                "error": error,
            }),
        );
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for bound in self.running.values() {
            bound.task.abort();
        }
        self.published.set(Vec::new(), None);
    }
}

/// Starts the home network route on `port`: loads or creates the host key, then
/// keeps one listener on each home address, rescanning interfaces periodically.
/// Aborting the returned task stops every listener. `published` follows the
/// addresses actually bound.
pub fn spawn(
    router: Router,
    data_dir: &Path,
    port: u16,
    logger: Arc<JsonlLogger>,
    published: HomeRoutes,
) -> io::Result<JoinHandle<()>> {
    let host_key = load_or_create_host_key(data_dir)?;
    let config = tls_config(&host_key).map_err(io::Error::other)?;
    let _ = logger.record(
        "info",
        "home_route_started",
        serde_json::json!({ "port": port, "hostKeyPin": host_key.pin() }),
    );
    let mut listeners = Listeners {
        port,
        published,
        host_key_pin: host_key.pin(),
        acceptor: TlsAcceptor::from(config),
        router,
        logger,
        running: HashMap::new(),
        failed: HashMap::new(),
    };
    Ok(tokio::spawn(async move {
        loop {
            listeners
                .reconcile(&home_addresses(&interface_addresses()))
                .await;
            tokio::time::sleep(RESCAN_INTERVAL).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// The prototype's output on linux-build-host (`interfaces.py`, plan §3.1) plus the
    /// rules it states but that VM had no example of.
    #[test]
    fn classifier_matches_the_prototype_table() {
        use Verdict::*;
        let cases: &[(&str, bool, &str, Verdict)] = &[
            // Prototype rows, linux-build-host 2026-10-17.
            ("lo", true, "127.0.0.1", LoopbackOrLinkLocal),
            ("enp1s0", true, "144.202.10.15", Public),
            (
                "enp1s0",
                true,
                "fe80::5400:6ff:fedf:4d8d",
                LoopbackOrLinkLocal,
            ),
            ("tailscale0", true, "100.115.139.68", TailscaleRange),
            (
                "tailscale0",
                true,
                "fd7a:115c:a1e0::532c:8b45",
                TailscaleRange,
            ),
            ("lxdbr0", false, "10.29.161.1", InterfaceDown),
            ("lxdbr0", false, "fd42:42c5:e62d:e962::1", InterfaceDown),
            // The same bridge once it is up is still not a home network.
            ("lxdbr0", true, "10.29.161.1", VirtualInterface),
            // Home networks.
            ("en0", true, "192.168.1.20", Advertise),
            ("eth0", true, "10.0.0.5", Advertise),
            ("wlan0", true, "172.16.4.2", Advertise),
            ("wlp2s0", true, "172.31.255.254", Advertise),
            ("en0", true, "fd12:3456:789a::1", Advertise),
            ("en0", false, "192.168.1.20", InterfaceDown),
            // Range edges.
            ("en0", true, "172.32.0.1", Public),
            ("en0", true, "100.63.255.255", Public),
            ("en0", true, "100.64.0.1", TailscaleRange),
            ("en0", true, "100.127.255.255", TailscaleRange),
            ("en0", true, "100.128.0.1", Public),
            ("en0", true, "169.254.10.1", LoopbackOrLinkLocal),
            ("en0", true, "0.0.0.0", LoopbackOrLinkLocal),
            ("en0", true, "::", LoopbackOrLinkLocal),
            ("en0", true, "::1", LoopbackOrLinkLocal),
            ("en0", true, "2001:db8::1", Public),
            ("en0", true, "fd7a:115c:a1e1::1", Advertise),
            // Virtual interfaces with private addresses.
            ("docker0", true, "172.17.0.1", VirtualInterface),
            ("br-1a2b3c", true, "172.18.0.1", VirtualInterface),
            ("veth12ab", true, "10.1.0.1", VirtualInterface),
            ("virbr0", true, "192.168.122.1", VirtualInterface),
            ("vEthernet (WSL)", true, "172.20.0.1", VirtualInterface),
            ("utun3", true, "fd00::1", VirtualInterface),
            ("wg0", true, "10.8.0.1", VirtualInterface),
            ("zt7nnig26", true, "10.147.17.1", VirtualInterface),
            ("bridge100", true, "192.168.64.1", VirtualInterface),
            ("awdl0", true, "fd00::2", VirtualInterface),
            ("llw0", true, "fd00::3", VirtualInterface),
        ];
        for (name, up, ip, expected) in cases {
            assert_eq!(
                classify(name, *up, ip.parse().unwrap()),
                *expected,
                "{name} {ip} up={up}"
            );
        }
    }

    #[test]
    fn home_addresses_keep_only_advertised_ones_once() {
        let entry = |name: &str, up: bool, ip: &str| InterfaceAddress {
            name: name.into(),
            up,
            ip: ip.parse().unwrap(),
        };
        let addresses = home_addresses(&[
            entry("enp1s0", true, "144.202.10.15"),
            entry("en0", true, "192.168.1.20"),
            entry("tailscale0", true, "100.115.139.68"),
            entry("en0", true, "fd12::1"),
            entry("en0", true, "192.168.1.20"),
            entry("docker0", true, "172.17.0.1"),
        ]);
        assert_eq!(
            addresses,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
                IpAddr::V6("fd12::1".parse::<Ipv6Addr>().unwrap()),
            ]
        );
        assert_eq!(origin(addresses[1], DEFAULT_PORT), "https://[fd12::1]:3778");
    }

    /// The Windows API returns native socket bytes, friendly UTF-16 names and
    /// operational state; only complete addresses on a real, up adapter qualify.
    #[cfg(windows)]
    #[test]
    fn windows_adapters_preserve_addresses_state_and_exclude_tunnels() {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            IF_TYPE_TUNNEL, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
        };
        use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusDown, IfOperStatusUp};
        use windows_sys::Win32::Networking::WinSock::{
            AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6,
        };

        let mut name: Vec<u16> = "Wi-Fi 日本語\0".encode_utf16().collect();
        let mut v4 = SOCKADDR_IN::default();
        v4.sin_family = AF_INET;
        v4.sin_addr.S_un.S_addr = u32::from_ne_bytes([192, 168, 1, 20]);
        let mut v6 = SOCKADDR_IN6::default();
        v6.sin6_family = AF_INET6;
        v6.sin6_addr.u.Byte = "fd12::1234".parse::<Ipv6Addr>().unwrap().octets();
        let mut ipv6 = IP_ADAPTER_UNICAST_ADDRESS_LH::default();
        ipv6.Address.lpSockaddr = (&mut v6 as *mut SOCKADDR_IN6).cast();
        ipv6.Address.iSockaddrLength = std::mem::size_of::<SOCKADDR_IN6>() as i32;
        let mut ipv4 = IP_ADAPTER_UNICAST_ADDRESS_LH::default();
        ipv4.Next = &mut ipv6;
        ipv4.Address.lpSockaddr = (&mut v4 as *mut SOCKADDR_IN).cast();
        ipv4.Address.iSockaddrLength = std::mem::size_of::<SOCKADDR_IN>() as i32;
        let mut adapter = IP_ADAPTER_ADDRESSES_LH::default();
        adapter.FriendlyName = name.as_mut_ptr();
        adapter.OperStatus = IfOperStatusUp;
        adapter.FirstUnicastAddress = &mut ipv4;
        // SAFETY: all native nodes and their addresses live on this stack.
        let read =
            |adapter: &IP_ADAPTER_ADDRESSES_LH| unsafe { windows_adapter_addresses(adapter) };
        let expected = vec![
            InterfaceAddress {
                name: "Wi-Fi 日本語".into(),
                up: true,
                ip: "192.168.1.20".parse().unwrap(),
            },
            InterfaceAddress {
                name: "Wi-Fi 日本語".into(),
                up: true,
                ip: "fd12::1234".parse().unwrap(),
            },
        ];
        assert_eq!(read(&adapter), expected);
        assert_eq!(
            home_addresses(&read(&adapter)),
            expected.iter().map(|e| e.ip).collect::<Vec<_>>()
        );
        adapter.OperStatus = IfOperStatusDown;
        assert!(read(&adapter).iter().all(|entry| !entry.up));
        assert!(home_addresses(&read(&adapter)).is_empty());
        adapter.OperStatus = IfOperStatusUp;
        adapter.IfType = IF_TYPE_TUNNEL;
        assert!(read(&adapter).is_empty());
        adapter.IfType = 0;
        ipv4.Address.iSockaddrLength = 2; // family present, truncated IPv4
        ipv6.Address.lpSockaddr = std::ptr::null_mut();
        ipv4.Next = &mut ipv6;
        adapter.FirstUnicastAddress = &mut ipv4;
        assert!(read(&adapter).is_empty());
    }

    #[test]
    fn host_key_is_created_private_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_host_key(dir.path()).unwrap();
        let again = load_or_create_host_key(dir.path()).unwrap();
        assert_eq!(first.pin(), again.pin());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(HOST_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(dir.path().join(HOST_KEY_FILE), b"short").unwrap();
        assert_eq!(
            load_or_create_host_key(dir.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn route_filter_keeps_pairing_page_mcp_and_loopback_routes_off_the_home_network() {
        for allowed in [
            "/healthz",
            "/readyz",
            "/api/v1/host/status",
            "/api/v1/rpc",
            "/api/v1/pairing/claim",
            "/api/v1/pairing/session",
        ] {
            assert!(home_route_allows(allowed), "{allowed}");
        }
        for refused in [
            "/",
            "/pair",
            "/pair/style.css",
            "/mcp",
            "/oauth/mcp/token",
            "/.well-known/oauth-authorization-server",
            "/api/v1/mcp/clients",
            "/api/v1/pairing/offers",
            "/api/v1/pairing/pending",
        ] {
            assert!(!home_route_allows(refused), "{refused}");
        }
    }

    /// A client that accepts exactly one SPKI pin, as the phone does.
    #[derive(Debug)]
    struct PinVerifier {
        pin: String,
        provider: Arc<rustls::crypto::CryptoProvider>,
    }

    impl rustls::client::danger::ServerCertVerifier for PinVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            let spki = spki_of(end_entity.as_ref());
            if wonder_api::pairing_protocol::host_key_pin(&spki) == self.pin {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("pin mismatch".into()))
            }
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// The SubjectPublicKeyInfo of a P-256 certificate: the fixed SPKI header
    /// followed by the 65-byte point, found by its header bytes.
    fn spki_of(certificate: &[u8]) -> Vec<u8> {
        let header: [u8; 26] = [
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        let start = certificate
            .windows(header.len())
            .position(|window| window == header)
            .expect("P-256 SPKI in certificate");
        certificate[start..start + header.len() + 65].to_vec()
    }

    /// A TLS connection that trusts only the certificate with `pin`, as a phone does.
    async fn pinned_tls(
        address: SocketAddr,
        pin: &str,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier {
                pin: pin.to_owned(),
                provider,
            }))
            .with_no_client_auth();
        let stream = TcpStream::connect(address).await.unwrap();
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect("wonder.invalid".try_into().unwrap(), stream)
            .await
            .map_err(|error| error.to_string())
    }

    async fn https_get(
        address: SocketAddr,
        pin: &str,
        path: &str,
        origin: Option<&str>,
    ) -> Result<(u16, String), String> {
        let mut tls = pinned_tls(address, pin).await?;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let origin = origin
            .map(|origin| format!("Origin: {origin}\r\n"))
            .unwrap_or_default();
        tls.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n{origin}Connection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut reply = String::new();
        tls.read_to_string(&mut reply).await.ok();
        let status = reply
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        Ok((status, reply))
    }

    /// Hold the conflicting socket through the failed-bind assertion. A low
    /// port avoids unrelated port-0 clients/listeners while exercising retry
    /// after release; the subsequent successful bind still owns its socket.
    fn held_port() -> std::net::TcpListener {
        const FIRST: u32 = 20_000;
        const COUNT: u32 = 12_000;
        let offset = std::process::id() % COUNT;
        (0..COUNT)
            .find_map(|step| {
                let port = (FIRST + (offset + step) % COUNT) as u16;
                std::net::TcpListener::bind(("127.0.0.1", port)).ok()
            })
            .expect("a free loopback port below the ephemeral range")
    }

    /// Host status offers only the addresses actually bound, and nothing once
    /// the listeners stop.
    #[tokio::test]
    async fn published_home_routes_follow_the_bound_listeners() {
        let dir = tempfile::tempdir().unwrap();
        let host_key = load_or_create_host_key(dir.path()).unwrap();
        let published = HomeRoutes::default();
        let mut listeners = Listeners {
            port: 0,
            published: published.clone(),
            host_key_pin: host_key.pin(),
            acceptor: TlsAcceptor::from(tls_config(&host_key).unwrap()),
            router: Router::new(),
            logger: Arc::new(JsonlLogger::new(dir.path().join("logs"), "test.jsonl").unwrap()),
            running: HashMap::new(),
            failed: HashMap::new(),
        };
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        listeners.reconcile(&[loopback]).await;
        let address = listeners.running[&loopback].address;
        assert_ne!(address.port(), 0);
        let port = address.port();
        assert_eq!(
            published.origins(),
            [origin(loopback, port)],
            "{:?}",
            listeners.failed
        );
        assert_eq!(published.host_key_pin(), Some(host_key.pin()));
        assert!(
            TcpStream::connect(address).await.is_ok(),
            "the published listener accepts connections"
        );

        // A port another program holds is not offered.
        listeners.reconcile(&[]).await;
        assert!(published.origins().is_empty());
        let taken = held_port();
        listeners.port = taken.local_addr().unwrap().port();
        listeners.reconcile(&[loopback]).await;
        assert!(published.origins().is_empty());
        assert_eq!(published.host_key_pin(), None);
        drop(taken);

        // Each reconcile retries the same configured port after its owner
        // releases it. The bound socket, rather than a prior reservation,
        // determines the published origin.
        for _ in 0..200 {
            listeners.reconcile(&[loopback]).await;
            if !published.origins().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(published.origins().len(), 1, "{:?}", listeners.failed);
        let address = listeners.running[&loopback].address;
        assert_eq!(address.port(), listeners.port);
        assert_eq!(
            published.origins(),
            [origin(address.ip(), address.port())],
            "{:?}",
            listeners.failed
        );
        assert!(TcpStream::connect(address).await.is_ok());
        drop(listeners);
        assert!(published.origins().is_empty());
        assert_eq!(published.host_key_pin(), None);
    }

    /// Sockets held open in the TLS handshake cannot exhaust the listener:
    /// past the cap new connections close at once, and room comes back as
    /// the held ones end.
    #[tokio::test]
    async fn held_connections_are_capped_and_released() {
        let dir = tempfile::tempdir().unwrap();
        let host_key = load_or_create_host_key(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route("/readyz", axum::routing::get(|| async { "ok" }));
        let task = tokio::spawn(serve(
            listener,
            TlsAcceptor::from(tls_config(&host_key).unwrap()),
            router,
            origin(address.ip(), address.port()).into(),
        ));
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(TcpStream::connect(address).await.unwrap());
        }
        // Let the listener accept them all before the next one arrives.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            https_get(address, &host_key.pin(), "/readyz", None)
                .await
                .is_err(),
            "a connection past the cap was served"
        );
        drop(held);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (status, _) = https_get(address, &host_key.pin(), "/readyz", None)
            .await
            .unwrap();
        assert_eq!(status, 200);
        task.abort();
    }

    #[tokio::test]
    async fn listener_serves_pinned_tls_only_and_tags_requests_with_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let host_key = load_or_create_host_key(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let home_origin = origin(address.ip(), address.port());
        let router = Router::new()
            .route(
                "/api/v1/echo",
                axum::routing::get(
                    |axum::Extension(route): axum::Extension<HomeRoute>| async move { route.origin },
                ),
            )
            .route("/pair", axum::routing::get(|| async { "pairing page" }));
        let task = tokio::spawn(serve(
            listener,
            TlsAcceptor::from(tls_config(&host_key).unwrap()),
            router,
            home_origin.clone().into(),
        ));

        let (status, reply) = https_get(address, &host_key.pin(), "/api/v1/echo", None)
            .await
            .unwrap();
        assert_eq!(status, 200, "{reply}");
        assert!(reply.ends_with(&home_origin), "{reply}");

        let (status, _) = https_get(address, &host_key.pin(), "/pair", None)
            .await
            .unwrap();
        assert_eq!(status, 404);

        let impostor = HostKey::generate().pin();
        let refused = https_get(address, &impostor, "/api/v1/echo", None).await;
        assert!(refused.is_err(), "{refused:?}");

        // Plain HTTP gets no HTTP reply: the TLS handshake fails and the socket closes.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut plain = tokio::net::TcpStream::connect(address).await.unwrap();
        plain
            .write_all(b"GET /api/v1/echo HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut reply = Vec::new();
        let _ = plain.read_to_end(&mut reply).await;
        assert!(
            !reply.starts_with(b"HTTP/"),
            "{:?}",
            String::from_utf8_lossy(&reply)
        );
        task.abort();
    }

    /// D1's contract with the phone: the pairing link pins the key the home
    /// listener's TLS and the host proof both use, and a proof holds only for
    /// the route origin and nonce it was made for.
    // Uses the RPC test fixtures, which are Unix-only (see lib.rs).
    #[cfg(unix)]
    #[tokio::test]
    async fn the_pairing_link_pin_verifies_the_host_proof_on_its_route_only() {
        use wonder_api::pairing_protocol::{verify_host_proof, HostProof, HostProofError};
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let tailnet = "https://wonder.example.ts.net";
        state.public_origin = tailnet.into();
        state.public_origin_file = None;
        let router = crate::router(state.clone());

        let (status, offer) = crate::test_support::call(
            &state,
            "POST",
            "/api/v1/pairing/offers",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{offer}");
        let url = offer["url"].as_str().unwrap();
        let fields: Vec<&str> = url.split_once('#').unwrap().1.split('&').collect();
        // The three fields older links had, then the pin.
        assert!(fields[0].starts_with("secret="), "{url}");
        assert!(fields[1].starts_with("offerId="), "{url}");
        assert!(fields[2].starts_with("hostInstallationId="), "{url}");
        let pin = fields[3].strip_prefix("hostKey=").expect("pin in link");
        assert_eq!(fields.len(), 4, "{url}");
        assert_eq!(offer["hostKeyPin"], pin);

        // The phone dials the home listener trusting only that pin.
        let host_key = load_or_create_host_key(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let home = origin(address.ip(), address.port());
        let task = tokio::spawn(serve(
            listener,
            TlsAcceptor::from(tls_config(&host_key).unwrap()),
            router.clone(),
            home.clone().into(),
        ));
        let nonce = "0123456789abcdef0123456789ABCDEF";
        let proof_path = format!("/api/v1/host/proof?nonce={nonce}");
        let (status, reply) = https_get(address, pin, &proof_path, Some(&home))
            .await
            .unwrap();
        assert_eq!(status, 200, "{reply}");
        let proof: HostProof =
            serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(verify_host_proof(pin, &home, nonce, &proof), Ok(()));
        // Replayed for another route or another challenge.
        assert_eq!(
            verify_host_proof(pin, tailnet, nonce, &proof),
            Err(HostProofError::InvalidSignature)
        );
        assert_eq!(
            verify_host_proof(pin, &home, "fedcba9876543210fedcba9876543210", &proof),
            Err(HostProofError::InvalidSignature)
        );
        // The home listener signs only for its own origin.
        let (status, _) = https_get(address, pin, &proof_path, Some(tailnet))
            .await
            .unwrap();
        assert_eq!(status, 403);
        let (status, _) = https_get(address, pin, &proof_path, None).await.unwrap();
        assert_eq!(status, 403);
        task.abort();

        // Over loopback (where Tailscale Serve delivers) it signs the tailnet
        // origin, and never a home origin.
        let get = |path: &str, origin: &str| {
            Request::builder()
                .uri(path)
                .header("origin", origin)
                .body(Body::empty())
                .unwrap()
        };
        let response = router
            .clone()
            .oneshot(get(&proof_path, tailnet))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let proof: HostProof = serde_json::from_slice(&body).unwrap();
        assert_eq!(verify_host_proof(pin, tailnet, nonce, &proof), Ok(()));
        let response = router
            .clone()
            .oneshot(get(&proof_path, &home))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // Only 32 hex digits: in particular never a sealed handshake's nonce,
        // which the same transcript would otherwise let a relay obtain.
        let sealed = crate::rpc::sealed::handshake_nonce(&[1; 32], &[2; 32]);
        for nonce in [
            sealed.as_str(),
            "0123456789abcdef",
            &format!("{nonce}0"),
            "g".repeat(32).as_str(),
        ] {
            let response = handle(
                router.clone(),
                &home,
                get(&format!("/api/v1/host/proof?nonce={nonce}"), &home),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{nonce}");
        }
        let response = handle(router.clone(), &home, get("/api/v1/host/proof", &home)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // A different key at this address cannot pass for the paired host.
        std::fs::write(
            dir.path().join(HOST_KEY_FILE),
            HostKey::generate().to_secret_bytes(),
        )
        .unwrap();
        let response = handle(router.clone(), &home, get(&proof_path, &home)).await;
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let swapped: HostProof = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            verify_host_proof(pin, &home, nonce, &swapped),
            Err(HostProofError::PinMismatch)
        );
    }

    /// A paired phone over a real home listener with the real router: the
    /// socket's challenge names that listener, its routed calls pass the origin
    /// check, and stopping the listener closes the established socket.
    // Uses the RPC test fixtures, which are Unix-only (see lib.rs).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_phone_signs_in_over_the_home_route_and_its_socket_ends_with_the_listener() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::{json, Value};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let key = crate::rpc::tests::paired_phone(&state).await;
        let host_key = load_or_create_host_key(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let home = origin(address.ip(), address.port());
        let task = tokio::spawn(serve(
            listener,
            TlsAcceptor::from(tls_config(&host_key).unwrap()),
            crate::router(state.clone()),
            home.clone().into(),
        ));

        // A native client's upgrade: session cookie, no Origin header.
        let tls = pinned_tls(address, &host_key.pin()).await.unwrap();
        let mut request = format!("wss://{address}/api/v1/rpc")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "cookie",
            "__Host-wonder_session=session-token".parse().unwrap(),
        );
        let (socket, _) = tokio_tungstenite::client_async(request, tls).await.unwrap();
        let (mut sink, mut stream) = socket.split();
        let mut call = async |id: u32, method: &str, params: Value| -> Value {
            let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            sink.send(Message::text(frame.to_string())).await.unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(60), stream.next())
                    .await
                    .expect("an answer")
                {
                    Some(Ok(Message::Text(text))) => {
                        return serde_json::from_str::<Value>(&text).unwrap()
                    }
                    Some(Ok(_)) => continue,
                    other => panic!("socket ended: {other:?}"),
                }
            }
        };
        let challenge = call(1, "auth.challenge", Value::Null).await;
        assert_eq!(challenge["result"]["origin"], home, "{challenge}");
        let auth = crate::rpc::tests::signed_challenge(&key, &challenge["result"]);
        let init = call(
            2,
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}, "auth": auth}),
        )
        .await;
        assert_eq!(init["result"]["authority"], "device", "{init}");
        let status = call(3, "host.status", Value::Null).await;
        crate::tests::validate_http_contract("hostStatus", &status["result"]);

        // The same over the sealed socket: its routed calls keep the listener's
        // route too, so the home origin still passes the origin check.
        let tls = pinned_tls(address, &host_key.pin()).await.unwrap();
        let mut request = format!("wss://{address}/api/v1/rpc/sealed")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", home.parse().unwrap());
        let (mut sealed, _) = tokio_tungstenite::client_async(request, tls).await.unwrap();
        let initiator = crate::rpc::sealed::Initiator::new().unwrap();
        sealed.send(Message::text(initiator.hello())).await.unwrap();
        let Some(Ok(Message::Text(welcome))) = sealed.next().await else {
            panic!("no welcome");
        };
        let (mut sealer, mut opener) = initiator
            .finish(welcome.as_str(), &host_key.pin(), &home)
            .unwrap();
        let opening = json!({"session": "session-token"}).to_string();
        sealed
            .send(Message::binary(sealer.seal(opening.as_bytes()).unwrap()))
            .await
            .unwrap();
        let mut sealed_call = async |id: u32, method: &str, params: Value| -> Value {
            let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let frame = sealer.seal(frame.to_string().as_bytes()).unwrap();
            sealed.send(Message::binary(frame)).await.unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(60), sealed.next())
                    .await
                    .expect("an answer")
                {
                    Some(Ok(Message::Binary(frame))) => {
                        return serde_json::from_slice::<Value>(&opener.open(&frame).unwrap())
                            .unwrap()
                    }
                    Some(Ok(_)) => continue,
                    other => panic!("sealed socket ended: {other:?}"),
                }
            }
        };
        let challenge = sealed_call(1, "auth.challenge", Value::Null).await;
        assert_eq!(challenge["result"]["origin"], home, "{challenge}");
        let auth = crate::rpc::tests::signed_challenge(&key, &challenge["result"]);
        let init = sealed_call(
            2,
            "initialize",
            json!({"protocolVersion": 1, "clientInfo": {"name": "phone"}, "auth": auth}),
        )
        .await;
        assert_eq!(init["result"]["authority"], "device", "{init}");
        let status = sealed_call(3, "host.status", Value::Null).await;
        crate::tests::validate_http_contract("hostStatus", &status["result"]);

        // The interface went away (or the route was turned off): the listener
        // task is aborted, and the upgraded socket must not outlive it.
        task.abort();
        let ended = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "the socket stayed open after its listener stopped"
        );
        let ended = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match sealed.next().await {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "the sealed socket outlived its listener");
    }

    /// Through the real router: a home listener's request is checked against that
    /// listener's origin only, and loopback authority cannot be claimed over it.
    // Uses the same Unix-only ingestion fixture as the RPC socket check above.
    #[cfg(unix)]
    #[tokio::test]
    async fn home_route_requests_trust_only_their_own_origin() {
        let (_dir, mut state) = crate::ingestion::tests::fixture().await;
        state.public_origin = "https://wonder.example.ts.net".into();
        state.public_origin_file = None;
        let router = crate::router(state.clone());
        let home = "https://192.168.1.20:3778";
        let send = |path: &str, origin: &str, capability: bool| {
            let mut request = Request::builder().uri(path).header("origin", origin);
            if capability {
                request =
                    request.header("x-wonder-loopback-capability", &state.loopback_capability);
            }
            request.body(Body::empty()).unwrap()
        };
        let status = |response: Response| response.status();

        // Origin accepted; the session check is what refuses it.
        assert_eq!(
            status(
                handle(
                    router.clone(),
                    home,
                    send("/api/v1/host/status", home, false)
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
        // The same request on loopback, where the home origin is not trusted.
        assert_eq!(
            status(
                router
                    .clone()
                    .oneshot(send("/api/v1/host/status", home, false))
                    .await
                    .unwrap()
            ),
            StatusCode::FORBIDDEN
        );
        // The Tailscale origin presented to the home listener.
        assert_eq!(
            status(
                handle(
                    router.clone(),
                    home,
                    send(
                        "/api/v1/host/status",
                        "https://wonder.example.ts.net",
                        false
                    )
                )
                .await
            ),
            StatusCode::FORBIDDEN
        );
        // Pairing claims pass the origin check over the home route (and fail later
        // only for the empty body).
        let claim = handle(
            router.clone(),
            home,
            Request::builder()
                .method("POST")
                .uri("/api/v1/pairing/claim")
                .header("origin", home)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_ne!(claim.status(), StatusCode::FORBIDDEN);
        assert_ne!(claim.status(), StatusCode::NOT_FOUND);
        // Loopback authority, the pairing-link reply and MCP are refused.
        assert_eq!(
            status(
                handle(
                    router.clone(),
                    home,
                    send("/api/v1/host/status", home, true)
                )
                .await
            ),
            StatusCode::FORBIDDEN
        );
        for path in [
            "/pair",
            "/",
            "/mcp",
            "/api/v1/mcp/clients",
            "/api/v1/pairing/offers",
        ] {
            assert_eq!(
                status(handle(router.clone(), home, send(path, home, true)).await),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    /// The loopback listener is where Tailscale Serve and the relay tunnel
    /// deliver requests, so it must not accept a home network origin (home
    /// traffic arrives only on its own listener), and a request that came over
    /// the relay address may present only the relay origin, as the home
    /// listener does for its own. 403 is the origin check refusing; 401 is the
    /// origin passing and the missing session refusing.
    #[cfg(unix)]
    #[tokio::test]
    async fn loopback_requests_present_only_their_own_routes_origin() {
        const TAILNET: &str = "https://wonder.example.ts.net";
        const HOME: &str = "https://192.168.1.20:3778";
        const RELAY: &str = "https://r-k7x2q.getcupola.app";
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let status_file = dir.path().join("origin-status.jsonl");
        std::fs::write(
            &status_file,
            format!(
                r#"{{"state":"ready","origin":"{TAILNET}","routes":[{{"kind":"home","origin":"{HOME}"}},{{"kind":"relay","origin":"{RELAY}"}}]}}"#
            ),
        )
        .unwrap();
        state.public_origin_file = Some(status_file);
        std::fs::write(
            state.data_dir.join(crate::relay::STATUS_FILE),
            format!(r#"{{"enabled":true,"state":"ready","label":"k7x2q","origin":"{RELAY}"}}"#),
        )
        .unwrap();
        let router = crate::router(state.clone());
        let send = |path: &str, host: &str, origin: Option<&str>| {
            let mut request = Request::builder().uri(path).header("host", host);
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let request = request.body(Body::empty()).unwrap();
            let router = router.clone();
            async move { router.oneshot(request).await.unwrap().status() }
        };
        let status_path = "/api/v1/host/status";
        for (host, origin, expected) in [
            // Each route with its own origin passes the origin check.
            (
                "wonder.example.ts.net",
                Some(TAILNET),
                StatusCode::UNAUTHORIZED,
            ),
            ("wonder.example.ts.net", None, StatusCode::UNAUTHORIZED),
            // The relay address serves only sealed RPC, whatever the origin.
            ("r-k7x2q.getcupola.app", Some(RELAY), StatusCode::NOT_FOUND),
            ("r-k7x2q.getcupola.app", None, StatusCode::NOT_FOUND),
            // A home origin on loopback, by Origin or by Host.
            ("wonder.example.ts.net", Some(HOME), StatusCode::FORBIDDEN),
            ("192.168.1.20:3778", Some(HOME), StatusCode::FORBIDDEN),
            ("192.168.1.20:3778", None, StatusCode::FORBIDDEN),
            (
                "r-k7x2q.getcupola.app",
                Some(TAILNET),
                StatusCode::NOT_FOUND,
            ),
            ("r-k7x2q.getcupola.app", Some(HOME), StatusCode::NOT_FOUND),
            (
                "R-K7X2Q.getcupola.app.:443",
                Some(TAILNET),
                StatusCode::NOT_FOUND,
            ),
            // The relay origin off the relay address: refused by its path
            // before its origin is checked.
            ("wonder.example.ts.net", Some(RELAY), StatusCode::NOT_FOUND),
        ] {
            assert_eq!(
                send(status_path, host, origin).await,
                expected,
                "Host {host}, Origin {origin:?}"
            );
        }
        // The origin-only routes (pairing, sealed RPC) follow the same rule;
        // on the relay address only sealed RPC is served at all.
        for path in ["/api/v1/rpc/sealed", "/api/v1/pairing/session"] {
            assert_eq!(
                send(path, "wonder.example.ts.net", Some(HOME)).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        let sealed = "/api/v1/rpc/sealed";
        for (origin, expected) in [(TAILNET, true), (HOME, true), (RELAY, false)] {
            assert_eq!(
                send(sealed, "r-k7x2q.getcupola.app", Some(origin)).await == StatusCode::FORBIDDEN,
                expected,
                "{origin}"
            );
        }
        assert_eq!(
            send(
                "/api/v1/pairing/session",
                "r-k7x2q.getcupola.app",
                Some(RELAY)
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }
}
