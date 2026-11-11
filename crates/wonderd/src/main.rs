use std::sync::Arc;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use wonder_api::{
    pairing_protocol::{DevicePublicKeyJwk, PairingState},
    HostEventEnvelope,
};
use wonder_store::Store;
use wonderd::{
    capability_from_environment, logging::JsonlLogger, router, AppState, RuntimeCatalog,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    wonderd::cupola_env::adopt();
    limit_malloc_arenas();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

/// glibc gives each busy thread its own heap arena and keeps freed pages in
/// them, so a long-running daemon's RSS climbs well above its live heap. Two
/// arenas kept the soak's RSS flat at the same throughput. Set before the
/// runtime starts its threads; an explicit MALLOC_ARENA_MAX still wins.
fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt only adjusts allocator tuning and runs before any
        // other thread exists.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // EXPERIMENTAL Windows host: the daemon and the agents it runs resolve
    // `~` through HOME; Windows names the profile USERPROFILE (which Codex
    // and Claude Code also use for ~/.codex and ~/.claude). Windows process
    // environment updates are thread-safe.
    #[cfg(windows)]
    if std::env::var_os("HOME").is_none() {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            std::env::set_var("HOME", profile);
        }
    }
    if std::env::args().nth(1).as_deref() == Some("--locate-runtime") {
        println!("{}", wonderd::provider_runtime::locate_codex().display());
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--verify-runtime") {
        let path = std::env::args_os().nth(2).ok_or("runtime path required")?;
        wonder_app_server::verify_runtime(Path::new(&path)).await?;
        println!("Runtime version and protocol verified");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--prepare-data-dir") {
        println!("{}", data_directory()?.display());
        return Ok(());
    }
    let startup_started = std::time::Instant::now();
    let data_dir = data_directory()?;
    tokio::fs::create_dir_all(&data_dir).await?;
    wonderd::configure_git_reads(&data_dir)?;
    wonderd::context_usage::configure(&data_dir);
    let host_installation_id = persistent_host_installation_id(&data_dir)?;
    let log_dir = log_directory()?;
    let logger = Arc::new(JsonlLogger::new(&log_dir, "wonderd.jsonl")?);
    logger.record(
        "info",
        "daemon_starting",
        serde_json::json!({
            "dataDirectory": data_dir,
            "logDirectory": log_dir,
            "modelUsage": "none_until_live_opt_in",
        }),
    )?;
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        data_dir.join("wonder.sqlite3").display()
    );
    let store = Store::connect(&database_url).await?;
    logger.record(
        "info",
        "startup_stage",
        serde_json::json!({ "stage": "store_ready", "elapsedMs": startup_started.elapsed().as_millis() }),
    )?;
    let capability = capability_from_environment()?;
    let started_at = unix_timestamp_ms().to_string();
    let host_epoch = hex::encode(Sha256::digest(
        format!("{started_at}:{}", uuid::Uuid::new_v4()).as_bytes(),
    ));

    store.start_event_epoch(&host_epoch).await?;

    wonderd::provider_runtime_config::hydrate(&store, &host_installation_id).await?;
    let codex_bin = wonderd::provider_runtime::locate_codex();
    let codex_state = effective_codex_home()?;
    let sensitive_root_paths = sensitive_roots(&codex_state)?;
    let sensitive_roots = sensitive_root_paths
        .iter()
        .map(|path| path.to_str().ok_or("non-UTF-8 sensitive path"))
        .collect::<Result<Vec<_>, _>>()?;
    let data_root = data_dir.to_str().ok_or("non-UTF-8 data path")?;
    let log_root = log_dir.to_str().ok_or("non-UTF-8 log path")?;
    let legacy_root = std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join("Library/Application Support/Wonder"));
    // The data directory's older names (`~/.wonder` links to `~/.cupola`)
    // reach the same files, so they are denied too.
    let wonder_root = std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".wonder"));
    let legacy_aliases: Vec<PathBuf> = [legacy_root, wonder_root]
        .into_iter()
        .flatten()
        .filter(|path| {
            path.is_symlink()
                && wonderd::canonical_path::canonicalize(path).ok()
                    == wonderd::canonical_path::canonicalize(&data_dir).ok()
        })
        .collect();
    let mut denied_root_refs = vec![data_root, log_root];
    denied_root_refs.extend(legacy_aliases.iter().filter_map(|path| path.to_str()));
    denied_root_refs.extend(sensitive_roots.iter().copied());
    let runtime_catalog = RuntimeCatalog {
        preferences: store
            .agent_default_preferences()
            .await?
            .into_iter()
            .collect(),
        owner_hidden: {
            let mut hidden: std::collections::HashMap<_, std::collections::HashSet<String>> =
                Default::default();
            for (family, id) in store.hidden_models().await? {
                hidden.entry(family).or_default().insert(id);
            }
            hidden
        },
        ..Default::default()
    };

    let (events, _) = tokio::sync::broadcast::channel::<HostEventEnvelope>(256);
    let (revocations, _) = tokio::sync::broadcast::channel::<String>(64);
    let mut pairing_state = PairingState::default();
    for device in store.list_owner_devices().await? {
        let public_key: DevicePublicKeyJwk = serde_json::from_str(&device.public_key_jwk)?;
        pairing_state.restore_device(
            device.id,
            public_key,
            device.revoked_at.is_some(),
            device.session_expires_at_ms,
        );
    }
    for session in store
        .list_active_sessions(unix_timestamp_ms() as u64)
        .await?
    {
        let csrf_hash: [u8; 32] = hex::decode(session.csrf_hash)
            .map_err(|_| "stored CSRF hash is not hexadecimal")?
            .try_into()
            .map_err(|_| "stored CSRF hash has invalid length")?;
        pairing_state.restore_session(
            session.token_hash,
            session.device_id,
            csrf_hash,
            session.expires_at_ms,
        );
    }
    let pairing = Arc::new(tokio::sync::Mutex::new(pairing_state.into()));
    let public_origin =
        std::env::var("WONDER_PUBLIC_ORIGIN").unwrap_or_else(|_| "https://wonder.invalid".into());
    let public_origin_file = std::env::var_os("WONDER_PUBLIC_ORIGIN_FILE").map(PathBuf::from);
    wonderd::secrets::cleanup_delivery(&data_dir)?;
    wonderd::secrets::hydrate(&store, &host_installation_id).await?;
    let claude = wonderd::claude::Runtime::configured(&data_dir, &store);
    let opencode = wonderd::claude::Runtime::opencode(&data_dir, &store);
    let cursor = wonderd::claude::Runtime::cursor(&data_dir, &store);
    let projects = wonderd::projects::ProjectRuntime::configured(
        codex_bin.clone(),
        env!("CARGO_PKG_VERSION").into(),
        &codex_state,
        &claude_config_home()?,
        wonderd::ingestion::notification_sink(store.clone()),
    );
    let state = AppState {
        claude,
        opencode,
        cursor,
        projects,
        ingestion: wonderd::ingestion::Ingestion::default(),
        store,
        logger: Arc::clone(&logger),
        loopback_capability: capability,
        host_epoch,
        started_at,
        events,
        journal_wake: Default::default(),
        revocations,
        pairing,
        public_origin,
        public_origin_file,
        home_routes: wonderd::lan::HomeRoutes::default(),
        host_installation_id,
        data_dir: data_dir.clone(),
        codex_home: codex_state.clone(),
        provider_reconnect: Default::default(),
        installed_providers: Default::default(),
        denied_roots: wonderd::with_firmlink_aliases(
            denied_root_refs
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
        ),
        dispatch_lock: Arc::new(tokio::sync::Mutex::new(())),
        dispatch_wake: Arc::default(),
        thread_waits: Arc::default(),
        update_admission: Arc::new(Default::default()),
        approval_lock: Arc::new(tokio::sync::Mutex::new(())),
        runtime_catalog: Arc::new(RwLock::new(runtime_catalog)),
        computer_use_enabled: std::env::var("WONDER_COMPUTER_USE_ENABLED").as_deref() == Ok("1"),
        computer_use_bin: std::env::var_os("WONDER_COMPUTER_USE_BIN").map(PathBuf::from),
        computer_supervisor: Arc::new(
            wonderd::computer_sessions::ComputerSessionSupervisor::default(),
        ),
        simulators: Arc::new(wonderd::simulators::Simulators::on_this_mac()),
        voice: Arc::new(wonderd::voice::Voice::keychain(&data_dir)),
        t3_code: Arc::new(wonderd::T3Code::keychain(&data_dir)),
        claude_app: Arc::new(wonderd::ClaudeApp::on_this_mac()),
        terminals: Arc::default(),
        live_activity: Arc::default(),
        sealed_handshakes: Default::default(),
        relay: wonderd::relay::Manager::new(
            data_dir.clone(),
            wonderd::relay::Environment::from_env(),
            Some(Arc::clone(&logger)),
        ),
    };
    let _terminal_reaper =
        wonderd::terminal::spawn_reaper(&state.terminals, state.revocations.subscribe());
    state
        .computer_supervisor
        .retire_durable_sessions(&state)
        .await;
    let notification_service = wonderd::ingestion::spawn(state.clone()).await;
    let scheduler_task = wonderd::spawn_automation_scheduler(state.clone());
    let auto_archive_task = wonderd::projects::auto_archive::spawn(state.clone());
    let usage_resume_task = wonderd::usage_resume::spawn(state.clone());
    let runtime_update_task = wonderd::provider_runtime::spawn_update_watcher(state.clone());
    let listen_addr =
        std::env::var("WONDER_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3777".into());
    let listen_addr: SocketAddr = listen_addr.parse()?;
    if !listen_addr.ip().is_loopback() {
        return Err("WONDER_LISTEN_ADDR must be a loopback address".into());
    }
    let listener = match tokio::net::TcpListener::bind(listen_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            logger.record(
                "error",
                "startup_failed",
                serde_json::json!({
                    "stage": "daemon_bind",
                    "error": error.to_string(),
                    "elapsedMs": startup_started.elapsed().as_millis(),
                }),
            )?;
            return Err(error.into());
        }
    };
    let dispatcher_task = wonderd::dispatch::spawn(state.clone()).await?;
    let push_task = wonderd::push::spawn(state.clone());
    let external_push_task = wonderd::external_push::spawn(state.clone());
    let mac_dialogs_task = wonderd::mac_dialogs::spawn(state.clone());
    let live_activity_task = wonderd::live_activity::spawn(state.clone());
    // The tunnel ends at this listener, so it starts once the daemon listens.
    state.relay.resume().await;
    logger.record(
        "info",
        "daemon_listening",
        serde_json::json!({ "listenAddress": listen_addr, "elapsedMs": startup_started.elapsed().as_millis() }),
    )?;
    // The home network route is opt-in: WONDER_LAN_PORT=3778 turns it on.
    let home_route_task = match std::env::var("WONDER_LAN_PORT") {
        Ok(port) => {
            let port: u16 = port
                .parse()
                .map_err(|_| "WONDER_LAN_PORT must be a port number")?;
            Some(wonderd::lan::spawn(
                router(state.clone()),
                &data_dir,
                port,
                logger.clone(),
                state.home_routes.clone(),
            )?)
        }
        Err(_) => None,
    };
    println!("wonderd listening on {listen_addr}");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    // Serving a Router directly rebuilds its whole route table, with an
    // AppState copy per route layer, for every accepted connection; the make
    // service hands each connection the one already-built router.
    let server = axum::serve(listener, router(state.clone()).into_make_service())
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        _ = shutdown_signal() => {
            let _ = stop_tx.send(());
            // A connected native event socket must not prevent logout/quit.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut server).await;
        }
    }
    drop(notification_service);
    if let Some(task) = home_route_task {
        task.abort();
    }
    scheduler_task.abort();
    auto_archive_task.abort();
    usage_resume_task.abort();
    runtime_update_task.abort();
    dispatcher_task.abort();
    push_task.abort();
    external_push_task.abort();
    mac_dialogs_task.abort();
    live_activity_task.abort();
    let _ = scheduler_task.await;
    state.computer_supervisor.shutdown(&state).await;
    state.terminals.close_all();
    state.relay.shutdown().await;
    // Let each provider finish saving its work before the daemon exits. They
    // all stop at once, Codex too, so together they fit the supervisor's
    // single stop deadline however many are running.
    let bridges = [&state.claude, &state.opencode, &state.cursor]
        .into_iter()
        .flatten()
        .map(|runtime| runtime.client.clone());
    let (stopped, ()) = tokio::join!(
        wonder_app_server::AppServerClient::shutdown_together(bridges),
        state.projects.shutdown(),
    );
    stopped?;
    logger.record("info", "daemon_stopped", serde_json::json!({}))?;
    Ok(())
}

fn data_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("WONDER_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
        let path = PathBuf::from(base).join("Wonder");
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME")?;
        Ok(wonderd::data_home::prepare(Path::new(&home))?)
    }
}

fn persistent_host_installation_id(data_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = data_dir.join("host-installation-id");
    match std::fs::read_to_string(&path) {
        Ok(value) => {
            let value = value.trim();
            if uuid::Uuid::parse_str(value).is_err() {
                return Err("stored host installation id is invalid".into());
            }
            return Ok(value.to_owned());
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }

    let generated = uuid::Uuid::new_v4().to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            writeln!(file, "{generated}")?;
            Ok(generated)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let value = std::fs::read_to_string(&path)?;
            let value = value.trim();
            if uuid::Uuid::parse_str(value).is_err() {
                return Err("stored host installation id is invalid".into());
            }
            Ok(value.to_owned())
        }
        Err(error) => Err(error.into()),
    }
}

fn log_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("WONDER_LOG_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    return Ok(data_directory()?.join("logs"));
    #[cfg(not(windows))]
    let home = std::env::var("HOME")?;
    #[cfg(target_os = "macos")]
    return Ok(PathBuf::from(home).join("Library/Logs/Wonder"));
    // Experimental headless host: keep logs beside the data, outside ~/Library.
    #[cfg(all(unix, not(target_os = "macos")))]
    return Ok(wonderd::data_home::default_dir(Path::new(&home)).join("logs"));
}

fn sensitive_roots(codex_state: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let home = PathBuf::from(std::env::var("HOME")?);
    Ok(wonderd::sensitive_roots(
        &home,
        &working_directory(),
        codex_state,
        &|name| std::env::var_os(name),
    ))
}

fn claude_config_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = PathBuf::from(std::env::var("HOME")?);
    Ok(wonderd::claude_config_home(
        &home,
        &working_directory(),
        &|name| std::env::var_os(name),
    ))
}

/// Relative tool settings resolve from here, as the host's own file access
/// does. A removed working folder must not stop the host from starting.
fn working_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn effective_codex_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // The owner's Codex home (Settings) is read once, here: thread identity,
    // activity and the protected folders all follow it, and every Codex launch
    // uses the same one. A change applies after the next restart.
    if let Some(path) = wonder_app_server::provider_environment::runtime_settings("codex")
        .home
        .filter(|path| path.is_dir())
    {
        wonder_app_server::provider_environment::set_active_codex_home(path.clone());
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn unix_timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis()
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    // EXPERIMENTAL Windows host: the supervisor sends CTRL_BREAK to the
    // daemon's process group; a console close or Ctrl-C also stops it.
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let mut brk = windows::ctrl_break().expect("install Ctrl-Break handler");
        let mut close = windows::ctrl_close().expect("install console close handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = brk.recv() => {},
            _ = close.recv() => {},
        }
    }
}
