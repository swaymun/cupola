//! Project-scoped vault. Values and their reference/grant metadata live only in
//! the login Keychain. No secret-bearing type implements Debug. Agent tools
//! receive references; only a bounded host-owned command receives plaintext.
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{LazyLock, RwLock as SyncRwLock};
use zeroize::{Zeroize, Zeroizing};

const UNAVAILABLE: &str = "Secrets need a computer with a login Keychain.";
#[cfg(target_os = "macos")]
const KEYCHAIN_ERROR: &str =
    "The login Keychain is unavailable. Unlock it on your computer and try again.";
const INVALID: &str = "This secret is unavailable. Ask for it again.";
static MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
// Kept after delete/rotation until this process ends: an already-running tool
// may still emit the old value. Never serialized, logged or sent to a provider.
type RedactionList = Vec<(String, Zeroizing<String>)>;
static REDACTIONS: LazyLock<SyncRwLock<RedactionList>> = LazyLock::new(Default::default);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretUse {
    pub conversation_id: String,
    pub provider: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SecretInfo {
    pub secret_ref: String,
    pub name: String,
    pub project_id: String,
    pub uses: Vec<SecretUse>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    info: SecretInfo,
    value: String,
}
impl Drop for Record {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

pub(crate) fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
/// Lifecycle words that appear as nested protocol fields. A secret equal to one
/// would rewrite routing state outside the restored paths, so it is refused.
const PROTOCOL_WORDS: [&str; 12] = [
    "completed",
    "failed",
    "declined",
    "accepted",
    "cancelled",
    "inProgress",
    "pending",
    "running",
    "error",
    "message",
    "assistant",
    "user",
];
fn service(namespace: &str, project: &str) -> String {
    let prefix = if cfg!(test) {
        "org.cupola.fixture-vault"
    } else {
        "org.cupola.vault.v1"
    };
    format!(
        "{prefix}.{}",
        hex::encode(Sha256::digest(format!("{namespace}\0{project}")))
    )
}
fn namespace(state: &AppState) -> String {
    // Each test database gets its own fixture service, including parallel tests.
    if cfg!(test) {
        state.data_dir.to_string_lossy().into_owned()
    } else {
        state.host_installation_id.clone()
    }
}
fn remember(record: &Record) {
    let mut values = REDACTIONS.write().unwrap_or_else(|e| e.into_inner());
    if !values
        .iter()
        .any(|(name, value)| name == &record.info.name && **value == record.value)
    {
        values.push((
            record.info.name.clone(),
            Zeroizing::new(record.value.clone()),
        ));
        values.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
    }
}

/// Whole-output redaction happens before size limiting and before a tool reply
/// reaches either provider. Match original text once so placeholders and
/// overlapping values cannot be rewritten into partial disclosures.
pub(crate) fn redact_text(text: &str) -> String {
    let values = REDACTIONS.read().unwrap_or_else(|e| e.into_inner());
    if values.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    while !remaining.is_empty() {
        let found = values
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .filter_map(|(name, v)| remaining.find(v.as_str()).map(|at| (at, name, v.len())))
            .min_by_key(|(at, _, len)| (*at, std::cmp::Reverse(*len)));
        let Some((at, name, len)) = found else {
            out.push_str(remaining);
            break;
        };
        out.push_str(&remaining[..at]);
        out.push_str(&format!("[secret:{name}]"));
        remaining = &remaining[at + len..];
    }
    out
}
/// Redacts every string value; keys are never renamed. Tool output can sit
/// under any field name, so nothing nested is exempt.
pub(crate) fn redact_json(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(values) => values.iter_mut().for_each(redact_json),
        Value::Object(values) => values.values_mut().for_each(redact_json),
        _ => {}
    }
}

/// `redact_json` for a history item: the item's own root identity (its id,
/// type and lifecycle) is protocol structure, so a short secret equal to
/// "completed" cannot break routing. Everything nested is still redacted.
pub(crate) fn redact_item(item: &mut Value) {
    let root: Vec<(&str, Value)> = ["id", "type", "status", "state", "itemId", "callId"]
        .into_iter()
        .filter_map(|key| {
            item.get(key)
                .filter(|value| value.is_string())
                .map(|value| (key, value.clone()))
        })
        .collect();
    redact_json(item);
    for (key, value) in root {
        item[key] = value;
    }
}

/// Protocol identity is not tool output. Preserve it when a short secret is
/// also an ordinary protocol word, otherwise completion and approval routing
/// would break. Arbitrary tool content is still redacted.
pub(crate) fn redact_notification(notification: &mut Value) {
    if REDACTIONS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
    {
        return;
    }
    let original = notification.clone();
    redact_json(notification);
    for key in ["method", "id", "_wonderRuntimeId"] {
        if let Some(value) = original.get(key) {
            notification[key] = value.clone();
        }
    }
    for key in [
        "threadId",
        "turnId",
        "itemId",
        "requestId",
        "tool",
        "callId",
        "_wonderRuntimeId",
        "_wonderRequestId",
    ] {
        if let Some(value) = original["params"].get(key) {
            notification["params"][key] = value.clone();
        }
    }
    for key in ["id", "type", "status", "state"] {
        if let Some(value) = original["params"]["item"].get(key) {
            notification["params"]["item"][key] = value.clone();
        }
        if let Some(value) = original["params"]["turn"].get(key) {
            notification["params"]["turn"][key] = value.clone();
        }
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    use super::*;
    use security_framework::{
        item::{ItemClass, ItemSearchOptions, Limit},
        os::macos::keychain::SecKeychain,
    };
    /// Test builds only: with `WONDER_TEST_KEYCHAIN_DIR` set (a run over ssh needs it), every
    /// fixture-vault call uses one throwaway unlocked keychain created in that directory instead of the
    /// login Keychain, which an ssh session cannot unlock. Production builds have no such path.
    #[cfg(test)]
    fn test_keychain() -> Option<Result<SecKeychain, String>> {
        use security_framework::os::macos::keychain::{CreateOptions, KeychainSettings};
        static CREATED: LazyLock<Option<Result<PathBuf, String>>> = LazyLock::new(|| {
            let dir = PathBuf::from(std::env::var_os("WONDER_TEST_KEYCHAIN_DIR")?);
            let path = dir.join("wonderd-tests.keychain-db");
            let made = (|| {
                let mut keychain = CreateOptions::new()
                    .password("wonderd-tests")
                    .create(&path)
                    .map_err(|e| {
                        format!("cannot create the test keychain in {}: {e}", dir.display())
                    })?;
                let mut never_lock = KeychainSettings::new();
                never_lock.set_lock_on_sleep(false);
                never_lock.set_lock_interval(None);
                keychain
                    .set_settings(&never_lock)
                    .map_err(|e| format!("cannot keep the test keychain unlocked: {e}"))?;
                Ok(path)
            })();
            Some(made)
        });
        Some(match CREATED.as_ref()? {
            Ok(path) => {
                SecKeychain::open(path).map_err(|e| format!("cannot open the test keychain: {e}"))
            }
            Err(message) => Err(message.clone()),
        })
    }
    fn login() -> Result<SecKeychain, String> {
        #[cfg(test)]
        if let Some(keychain) = test_keychain() {
            return keychain;
        }
        let home = std::env::var_os("HOME").ok_or(KEYCHAIN_ERROR)?;
        SecKeychain::open(PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
            .map_err(|_| KEYCHAIN_ERROR.into())
    }
    pub(super) fn read(service: &str, name: &str) -> Result<Option<Record>, String> {
        match login()?.find_generic_password(service, name) {
            Ok((bytes, _)) => serde_json::from_slice(bytes.as_ref())
                .map(Some)
                .map_err(|_| INVALID.into()),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => Err(KEYCHAIN_ERROR.into()),
        }
    }
    pub(super) fn write(service: &str, record: &Record) -> Result<(), String> {
        let bytes = Zeroizing::new(serde_json::to_vec(record).map_err(|_| INVALID)?);
        login()?
            .set_generic_password(service, &record.info.name, &bytes)
            .map_err(|_| KEYCHAIN_ERROR.into())
    }
    pub(super) fn names(service: &str) -> Result<Vec<String>, String> {
        // Attribute-only search, restricted to this exact Project service.
        let found = ItemSearchOptions::new()
            .keychains(&[login()?])
            .class(ItemClass::generic_password())
            .service(service)
            .load_attributes(true)
            .load_data(false)
            .limit(Limit::All)
            .search();
        match found {
            Ok(items) => Ok(items
                .into_iter()
                .filter_map(|i| i.simplify_dict()?.remove("acct"))
                .collect()),
            Err(e) if e.code() == -25300 => Ok(vec![]),
            Err(_) => Err(KEYCHAIN_ERROR.into()),
        }
    }
    pub(super) fn delete(service: &str, name: &str) -> Result<(), String> {
        let result = ItemSearchOptions::new()
            .keychains(&[login()?])
            .class(ItemClass::generic_password())
            .service(service)
            .account(name)
            .delete();
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(_) => Err(KEYCHAIN_ERROR.into()),
        }
    }
}
#[cfg(not(target_os = "macos"))]
mod keychain {
    use super::*;
    pub(super) fn read(_: &str, _: &str) -> Result<Option<Record>, String> {
        Err(UNAVAILABLE.into())
    }
    pub(super) fn write(_: &str, _: &Record) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
    pub(super) fn names(_: &str) -> Result<Vec<String>, String> {
        Err(UNAVAILABLE.into())
    }
    pub(super) fn delete(_: &str, _: &str) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| "The vault could not finish. Try again.".to_owned())?
}

pub(crate) async fn existing(
    state: &AppState,
    project: &str,
    name: &str,
    conversation: &str,
) -> Result<Option<SecretInfo>, String> {
    let service = service(&namespace(state), project);
    let name = name.to_owned();
    let conversation = conversation.to_owned();
    blocking(move || {
        let Some(record) = keychain::read(&service, &name)? else {
            return Ok(None);
        };
        remember(&record);
        Ok(record
            .info
            .uses
            .iter()
            .any(|u| u.conversation_id == conversation)
            .then(|| record.info.clone()))
    })
    .await
}
fn value_error(name: &str, value: &str) -> Option<&'static str> {
    if !valid_name(name) || value.is_empty() || value.len() > 16384 || value.contains('\0') {
        return Some("Enter a value of at most 16 KB without a null character.");
    }
    PROTOCOL_WORDS
        .contains(&value)
        .then_some("Choose a value that is not a plain status word like “completed”.")
}
pub(crate) async fn save(
    state: &AppState,
    project: &str,
    name: &str,
    conversation: &str,
    provider: &str,
    value: &str,
) -> Result<SecretInfo, String> {
    if let Some(reason) = value_error(name, value) {
        return Err(reason.into());
    }
    let _guard = MUTATION.lock().await;
    let service = service(&namespace(state), project);
    let (name, project, conversation, provider, value) = (
        name.to_owned(),
        project.to_owned(),
        conversation.to_owned(),
        provider.to_owned(),
        Zeroizing::new(value.to_owned()),
    );
    blocking(move || {
        let previous = keychain::read(&service, &name)?;
        if let Some(previous) = &previous {
            remember(previous);
        }
        let mut info = previous
            .as_ref()
            .map(|r| r.info.clone())
            .unwrap_or(SecretInfo {
                secret_ref: format!("secretRef:{}", uuid::Uuid::new_v4()),
                name,
                project_id: project,
                uses: vec![],
            });
        if !info.uses.iter().any(|u| u.conversation_id == conversation) {
            info.uses.push(SecretUse {
                conversation_id: conversation,
                provider,
            });
        }
        let record = Record {
            info: info.clone(),
            value: value.to_string(),
        };
        keychain::write(&service, &record)?;
        remember(&record);
        Ok(info)
    })
    .await
}

async fn records(state: &AppState, project: &str) -> Result<Vec<Record>, String> {
    let service = service(&namespace(state), project);
    blocking(move || {
        let mut records = vec![];
        for name in keychain::names(&service)? {
            if let Some(record) = keychain::read(&service, &name)? {
                remember(&record);
                records.push(record);
            }
        }
        Ok(records)
    })
    .await
}
const HYDRATE_RETRY_SECONDS: [u64; 4] = [30, 120, 600, 1800];
/// Warm redaction before any provider is started. A Keychain that cannot be
/// read (locked, SSH session, access prompt after a rebuild) must not stop the
/// host: with no vault in use there is nothing to protect. Hydration is retried
/// in the background, and every vault read also remembers the values it sees.
pub async fn hydrate(store: &Store, namespace: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    if hydrate_once(store, namespace).await.is_ok() {
        return Ok(());
    }
    eprintln!("The login Keychain could not be read yet; the vault will retry.");
    let (store, namespace) = (store.clone(), namespace.to_owned());
    tokio::spawn(async move {
        // Bounded with backoff: a denied Keychain access prompt must not
        // re-prompt forever. After the last try, vault reads still remember
        // the values they see.
        for delay in HYDRATE_RETRY_SECONDS {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            if hydrate_once(&store, &namespace).await.is_ok() {
                return;
            }
        }
        eprintln!("The login Keychain stayed unreadable; the vault stopped retrying.");
    });
    Ok(())
}
async fn hydrate_once(store: &Store, namespace: &str) -> Result<(), String> {
    for project in store
        .list_projects()
        .await
        .map_err(|_| "Projects could not be read.")?
    {
        let service = service(namespace, &project.id);
        blocking(move || {
            for name in keychain::names(&service)? {
                if let Some(record) = keychain::read(&service, &name)? {
                    remember(&record);
                }
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

pub(crate) async fn list(State(state): State<AppState>, _: Extension<OwnerAuthority>) -> Response {
    if !cfg!(target_os = "macos") {
        return (StatusCode::NOT_IMPLEMENTED, UNAVAILABLE).into_response();
    }
    let projects = match state.store.list_projects().await {
        Ok(p) => p,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let mut secrets = vec![];
    for project in projects {
        match records(&state, &project.id).await {
            Ok(records) => {
                for record in records {
                    let mut entry = serde_json::to_value(&record.info).unwrap();
                    entry["projectName"] = json!(project.name);
                    for usage in entry["uses"].as_array_mut().unwrap() {
                        let id = usage["conversationId"].as_str().unwrap_or_default();
                        let title = state
                            .store
                            .project_conversation(id)
                            .await
                            .ok()
                            .flatten()
                            .map(|c| c.title);
                        usage["title"] = json!(title.unwrap_or_else(|| "Deleted chat".into()));
                    }
                    secrets.push(entry);
                }
            }
            Err(why) => return (StatusCode::SERVICE_UNAVAILABLE, why).into_response(),
        }
    }
    Json(json!({"secrets":secrets})).into_response()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeleteRequest {
    project_id: String,
    secret_ref: String,
}
pub(crate) async fn delete(
    State(state): State<AppState>,
    _: Extension<OwnerAuthority>,
    Json(request): Json<DeleteRequest>,
) -> Response {
    let _guard = MUTATION.lock().await;
    let records = match records(&state, &request.project_id).await {
        Ok(r) => r,
        Err(why) => return (StatusCode::SERVICE_UNAVAILABLE, why).into_response(),
    };
    let Some(record) = records
        .into_iter()
        .find(|r| r.info.secret_ref == request.secret_ref)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let service = service(&namespace(&state), &request.project_id);
    let name = record.info.name.clone();
    match blocking(move || keychain::delete(&service, &name)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => (StatusCode::SERVICE_UNAVAILABLE, why).into_response(),
    }
}

pub(crate) struct CommandRequest<'a> {
    pub reference: &'a str,
    pub delivery: &'a str,
    pub variable: &'a str,
    pub command: &'a str,
    pub seconds: u64,
}

pub(crate) async fn run(
    state: &AppState,
    project: &str,
    conversation: &str,
    request: CommandRequest<'_>,
) -> Result<Value, String> {
    let CommandRequest {
        reference,
        delivery,
        variable,
        command,
        seconds,
    } = request;
    if !cfg!(target_os = "macos") {
        return Err(UNAVAILABLE.into());
    }
    let record = records(state, project)
        .await?
        .into_iter()
        .find(|r| {
            r.info.secret_ref == reference
                && r.info.project_id == project
                && r.info
                    .uses
                    .iter()
                    .any(|u| u.conversation_id == conversation)
        })
        .ok_or(INVALID)?;
    let owner = state
        .store
        .project_conversation(conversation)
        .await
        .map_err(|_| INVALID)?
        .ok_or(INVALID)?;
    if owner.wonder_archived || owner.project_id != project || owner.access_mode != "full_access" {
        return Err(
            "Running a command with a secret requires this chat's Full access permission.".into(),
        );
    }
    if !matches!(delivery, "file" | "env")
        || !valid_variable(variable)
        || !(1..=120).contains(&seconds)
        || command.is_empty()
        || command.len() > 16384
    {
        return Err("Choose file or env delivery, a variable name, a command and a timeout from 1 to 120 seconds.".into());
    }
    let root = state.data_dir.join("secret-delivery");
    refuse_readable_root(&root, &owner.cwd, &shared_temporary_roots())?;
    run_command(
        &root,
        &owner.cwd,
        command,
        variable,
        delivery,
        &record.value,
        seconds,
    )
    .await
}
fn valid_variable(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        && !matches!(
            s,
            "PATH" | "HOME" | "SHELL" | "TMPDIR" | "ENV" | "BASH_ENV" | "ZDOTDIR" | "IFS"
        )
        && !s.starts_with("DYLD_")
        && !s.starts_with("LD_")
}

/// Temporary folders every agent sandbox may read or write.
fn shared_temporary_roots() -> Vec<std::path::PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    roots.extend(["/tmp", "/var/tmp"].map(std::path::PathBuf::from));
    roots
}

/// Canonical form of a path that may not exist yet: the deepest existing
/// ancestor is resolved, the rest is appended.
fn resolve_lenient(path: &std::path::Path) -> std::path::PathBuf {
    let mut rest = vec![];
    let mut current = path;
    loop {
        if let Ok(real) = crate::canonical_path::canonicalize(current) {
            return rest.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// A delivered secret file must live where an agent's sandbox cannot read it:
/// not inside the chat's folder and not in a shared temporary folder.
fn refuse_readable_root(
    root: &std::path::Path,
    cwd: &str,
    shared: &[std::path::PathBuf],
) -> Result<(), String> {
    let root = resolve_lenient(root);
    let readable = std::iter::once(std::path::PathBuf::from(cwd)).chain(shared.iter().cloned());
    if readable
        .map(|path| resolve_lenient(&path))
        .any(|path| root.starts_with(&path))
    {
        return Err("Secret delivery needs a private folder outside the chat's folder and shared temporary folders.".into());
    }
    Ok(())
}

/// The folder holding every run's private directory: created 0700, owned by
/// this user, never a link, and tightened to 0700 if an older one was looser.
#[cfg(unix)]
fn prepare_root(root: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    const FAIL: &str = "Secret delivery could not start.";
    if let Some(parent) = root.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| FAIL)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(FAIL.into()),
    }
    let metadata = std::fs::symlink_metadata(root).map_err(|_| FAIL)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(FAIL.into());
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|_| FAIL)?;
    }
    Ok(())
}

#[cfg(unix)]
async fn run_command(
    temporary_root: &std::path::Path,
    cwd: &str,
    command: &str,
    variable: &str,
    delivery: &str,
    value: &str,
    seconds: u64,
) -> Result<Value, String> {
    use std::{io::Write, os::unix::process::CommandExt, process::Stdio};
    use tokio::io::AsyncReadExt;
    // tempfile uses O_EXCL and mode 0600 in a private 0700 directory. Its
    // RAII lifetime includes failure, timeout and cancellation of this future.
    use std::os::unix::fs::PermissionsExt;
    prepare_root(temporary_root)?;
    // The per-run folder is created 0700 explicitly: the tempfile default
    // follows the process umask (775 on Linux).
    let directory = tempfile::Builder::new()
        .prefix(&format!("command-{}-", std::process::id()))
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(temporary_root)
        .map_err(|_| "Secret delivery could not start.")?;
    let mut file = None;
    let injected = if delivery == "file" {
        let mut target = tempfile::NamedTempFile::new_in(directory.path())
            .map_err(|_| "Secret delivery could not start.")?;
        target
            .write_all(value.as_bytes())
            .map_err(|_| "Secret delivery could not start.")?;
        let path = target.path().to_string_lossy().into_owned();
        file = Some(target);
        path
    } else {
        value.to_owned()
    };
    let injected = Zeroizing::new(injected);
    let mut child = tokio::process::Command::new("/bin/sh");
    child
        .args(["-c", command])
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env(variable, injected.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    child.as_std_mut().process_group(0);
    let mut child = child
        .spawn()
        .map_err(|_| "The secret command could not start.")?;
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
        }
    }
    let group = Group(child.id().ok_or("The secret command could not start.")?);
    async fn read(
        mut stream: impl tokio::io::AsyncRead + Unpin,
    ) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut bytes = Zeroizing::new(vec![]);
        (&mut stream)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Command output could not be read.")?;
        if bytes.len() > 1024 * 1024 {
            return Err("The secret command exceeded its output limit.".into());
        }
        Ok(bytes)
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        let (out, err, status) = tokio::try_join!(read(stdout), read(stderr), async { child.wait().await.map_err(|_| "The secret command failed.".to_owned()) })?;
        Ok::<_, String>(json!({"exitCode":status.code(), "stdout":redact_text(&String::from_utf8_lossy(&out)), "stderr":redact_text(&String::from_utf8_lossy(&err))}))
    }).await.map_err(|_| "The secret command timed out.".to_owned()).and_then(|r| r);
    drop(group); // descendants cannot keep reading the delivery file after return
    let _ = child.wait().await;
    drop(file);
    drop(directory);
    result
}
#[cfg(not(unix))]
async fn run_command(
    _: &std::path::Path,
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: u64,
) -> Result<Value, String> {
    Err(UNAVAILABLE.into())
}

/// A fixture service guard, never an owner's service. Cleans up even on panic.
#[cfg(all(test, target_os = "macos"))]
pub(crate) struct TestItem {
    service: String,
    name: String,
}
#[cfg(all(test, target_os = "macos"))]
impl TestItem {
    pub(crate) fn new(state: &AppState, project: &str, name: &str) -> Self {
        Self {
            service: service(&namespace(state), project),
            name: name.into(),
        }
    }
}
#[cfg(all(test, target_os = "macos"))]
impl Drop for TestItem {
    fn drop(&mut self) {
        let _ = keychain::delete(&self.service, &self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_redaction_rewrites_nested_strings_and_keeps_item_root_identity() {
        remember(&Record {
            info: SecretInfo {
                secret_ref: "fixture".into(),
                name: "SHORT_TOKEN".into(),
                project_id: "fixture".into(),
                uses: vec![],
            },
            value: "zq".into(),
        });
        let mut item = json!({"id":"zq","itemId":"zq-1","type":"zq","zq":"kept key","output":"has zq inside","nested":[{"callId":"zq","status":"zq","id":"zq","text":"zq"}]});
        redact_item(&mut item);
        assert_eq!(item["id"], "zq");
        assert_eq!(item["itemId"], "zq-1");
        assert_eq!(item["type"], "zq");
        assert_eq!(item["zq"], "kept key");
        assert_eq!(item["output"], "has [secret:SHORT_TOKEN] inside");
        // Nested fields under identity-looking names are tool output: redacted.
        assert_eq!(item["nested"][0]["callId"], "[secret:SHORT_TOKEN]");
        assert_eq!(item["nested"][0]["status"], "[secret:SHORT_TOKEN]");
        assert_eq!(item["nested"][0]["id"], "[secret:SHORT_TOKEN]");
        assert_eq!(item["nested"][0]["text"], "[secret:SHORT_TOKEN]");
    }
    #[test]
    fn secrets_refuse_values_that_equal_protocol_words() {
        assert!(value_error("TEST_TOKEN", "completed").is_some());
        assert!(value_error("TEST_TOKEN", "abc123").is_none());
        assert!(value_error("TEST_TOKEN", "").is_some());
    }
    #[tokio::test]
    async fn secrets_redaction_is_before_history_logs_and_output_limits() {
        let directory = tempfile::tempdir().unwrap();
        let value = uuid::Uuid::new_v4().to_string();
        let record = Record {
            info: SecretInfo {
                secret_ref: "fixture".into(),
                name: "TEST_TOKEN".into(),
                project_id: "fixture".into(),
                uses: vec![],
            },
            value: value.clone(),
        };
        remember(&record);
        let mut output = json!({"tool": [format!("before {value} after"), {"text": &value}]});
        redact_json(&mut output);
        assert!(!output.to_string().contains(&value));
        assert!(output.to_string().contains("[secret:TEST_TOKEN]"));
        assert!(!crate::history::sanitize_ansi_and_secrets(&value, 8).contains(&value[..8]));
        let logger = crate::logging::JsonlLogger::new(directory.path(), "vault.jsonl").unwrap();
        logger
            .record("info", "output", json!({"output":value}))
            .unwrap();
        assert!(
            !std::fs::read_to_string(directory.path().join("vault.jsonl"))
                .unwrap()
                .contains(&value)
        );
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("state.db").display()
        ))
        .await
        .unwrap();
        crate::ingestion::notification_sink(store)(
            json!({"method":"item/completed","params":{"item":{"output":value}}}),
        )
        .await
        .unwrap();
        for name in ["state.db", "state.db-wal"] {
            assert!(!std::fs::read(directory.path().join(name))
                .unwrap_or_default()
                .windows(value.len())
                .any(|w| w == value.as_bytes()));
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn secrets_file_and_env_delivery_redact_and_cleanup_on_exit_timeout_and_cancel() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let value = uuid::Uuid::new_v4().to_string();
        remember(&Record {
            info: SecretInfo {
                secret_ref: "fixture".into(),
                name: "DELIVERY_TOKEN".into(),
                project_id: "fixture".into(),
                uses: vec![],
            },
            value: value.clone(),
        });
        // Only the file path and mode are written to test-owned disk.
        let command = "printf '%s' \"$TOKEN_FILE\" > delivery-path; { stat -c '%a' \"$TOKEN_FILE\" 2>/dev/null || stat -f '%Lp' \"$TOKEN_FILE\"; }; wc -c < \"$TOKEN_FILE\"; cat \"$TOKEN_FILE\"; cat \"$TOKEN_FILE\" >&2";
        let output = run_command(
            &directory.path().join("secret-delivery"),
            root,
            command,
            "TOKEN_FILE",
            "file",
            &value,
            5,
        )
        .await
        .unwrap();
        assert!(output["stdout"].as_str().unwrap().starts_with("600\n"));
        assert!(output["stdout"]
            .as_str()
            .unwrap()
            .contains(&value.len().to_string()));
        assert!(output["stdout"]
            .as_str()
            .unwrap()
            .contains("[secret:DELIVERY_TOKEN]"));
        assert!(!output.to_string().contains(&value));
        let path = std::fs::read_to_string(directory.path().join("delivery-path")).unwrap();
        assert!(!std::path::Path::new(&path).exists());
        let env = run_command(
            &directory.path().join("secret-delivery"),
            root,
            "printf '%s' \"$TOKEN\"; exit 7",
            "TOKEN",
            "env",
            &value,
            5,
        )
        .await
        .unwrap();
        assert_eq!(env["exitCode"], 7);
        assert_eq!(env["stdout"], "[secret:DELIVERY_TOKEN]");
        assert!(run_command(
            &directory.path().join("secret-delivery"),
            root,
            "printf '%s' \"$TOKEN_FILE\" > delivery-path; sleep 20",
            "TOKEN_FILE",
            "file",
            &value,
            1
        )
        .await
        .is_err());
        let path = std::fs::read_to_string(directory.path().join("delivery-path")).unwrap();
        assert!(!std::path::Path::new(&path).exists());
        let root = root.to_owned();
        let run = tokio::spawn(async move {
            run_command(
                &std::path::Path::new(&root).join("secret-delivery"),
                &root,
                "printf '%s' \"$TOKEN_FILE\" > cancel-path; sleep 20",
                "TOKEN_FILE",
                "file",
                &value,
                30,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !directory.path().join("cancel-path").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        run.abort();
        let _ = run.await;
        let path = std::fs::read_to_string(directory.path().join("cancel-path")).unwrap();
        assert!(!std::path::Path::new(&path).exists());
    }
    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn secret_file_lives_in_a_private_run_folder_and_a_loose_root_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("secret-delivery");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = "m() { stat -c '%a' \"$1\" 2>/dev/null || stat -f '%Lp' \"$1\"; }; m \"$(dirname \"$(dirname \"$TOKEN_FILE\")\")\"; m \"$(dirname \"$TOKEN_FILE\")\"; m \"$TOKEN_FILE\"";
        let output = run_command(
            &root,
            directory.path().to_str().unwrap(),
            command,
            "TOKEN_FILE",
            "file",
            "fixture-value",
            5,
        )
        .await
        .unwrap();
        // root, per-run folder, file
        assert_eq!(output["stdout"], "700\n700\n600\n");
        assert_eq!(mode(&root), 0o700);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn secret_delivery_refuses_a_linked_root_and_writes_nothing_through_it() {
        let directory = tempfile::tempdir().unwrap();
        let elsewhere = directory.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let root = directory.path().join("secret-delivery");
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        let result = run_command(
            &root,
            directory.path().to_str().unwrap(),
            "true",
            "TOKEN_FILE",
            "file",
            "fixture-value",
            5,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }
    #[cfg(unix)]
    #[test]
    fn startup_cleanup_never_follows_a_linked_root_and_keeps_live_hosts_files() {
        let directory = tempfile::tempdir().unwrap();
        let stale = format!("command-{}-abandoned", i32::MAX - 1);
        let live = format!("command-{}-running", std::process::id());
        let real = directory.path().join("real");
        std::fs::create_dir_all(real.join(&stale)).unwrap();
        std::os::unix::fs::symlink(&real, directory.path().join("secret-delivery")).unwrap();
        assert!(cleanup_delivery(directory.path()).is_err());
        assert!(real.join(&stale).exists(), "a link must not be followed");
        std::fs::remove_file(directory.path().join("secret-delivery")).unwrap();
        let root = directory.path().join("secret-delivery");
        std::fs::create_dir_all(root.join(&stale)).unwrap();
        std::fs::create_dir_all(root.join(&live)).unwrap();
        cleanup_delivery(directory.path()).unwrap();
        assert!(!root.join(&stale).exists());
        assert!(root.join(&live).exists());
    }
    #[test]
    fn secret_delivery_root_must_sit_outside_readable_folders() {
        let base = tempfile::tempdir().unwrap();
        let (chat, shared, private) = (
            base.path().join("chat"),
            base.path().join("shared"),
            base.path().join("private"),
        );
        for folder in [&chat, &shared, &private] {
            std::fs::create_dir(folder).unwrap();
        }
        let shared_roots = [shared.clone()];
        let cwd = chat.to_str().unwrap();
        // Not created yet: still judged by where it would be.
        assert!(refuse_readable_root(&chat.join("a/secret-delivery"), cwd, &shared_roots).is_err());
        assert!(refuse_readable_root(&shared.join("secret-delivery"), cwd, &shared_roots).is_err());
        assert!(refuse_readable_root(&private.join("secret-delivery"), cwd, &shared_roots).is_ok());
        #[cfg(unix)]
        {
            let through = base.path().join("through");
            std::os::unix::fs::symlink(&chat, &through).unwrap();
            assert!(
                refuse_readable_root(&through.join("secret-delivery"), cwd, &shared_roots).is_err()
            );
        }
    }
}

/// Redact streamed output without persisting token fragments. Only a suffix
/// that could start a known value is held; ordinary text still streams. A
/// completed item/turn flushes its unmatched suffix before the terminal event.
#[derive(Clone, Default)]
pub(crate) struct NotificationRedactor {
    pending: std::collections::HashMap<String, (Value, Zeroizing<String>)>,
}
impl NotificationRedactor {
    pub(crate) fn ingest(&mut self, mut notification: Value) -> Vec<Value> {
        if REDACTIONS
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return vec![notification];
        }
        let method = notification["method"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let params = &notification["params"];
        let thread = params.get("threadId").cloned().unwrap_or(Value::Null);
        let turn = params.get("turnId").cloned().unwrap_or(Value::Null);
        let item = params.get("itemId").cloned().unwrap_or(Value::Null);
        let runtime = notification
            .get("_wonderRuntimeId")
            .cloned()
            .unwrap_or(Value::Null);
        let key = json!([runtime, thread, turn, item, method]).to_string();
        let mut ready = vec![];
        if method.ends_with("/delta") || method.ends_with("Delta") {
            if let Some(delta) = notification["params"]["delta"].as_str() {
                let mut text = self
                    .pending
                    .remove(&key)
                    .map(|(_, tail)| tail)
                    .unwrap_or_default();
                text.push_str(delta);
                let values = REDACTIONS.read().unwrap_or_else(|e| e.into_inner());
                let mut safe = String::new();
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let found = values
                        .iter()
                        .filter(|(_, v)| !v.is_empty())
                        .filter_map(|(name, v)| rest.find(v.as_str()).map(|at| (at, name, v.len())))
                        .min_by_key(|(at, _, len)| (*at, std::cmp::Reverse(*len)));
                    if let Some((at, name, len)) = found {
                        safe.push_str(&rest[..at]);
                        safe.push_str(&format!("[secret:{name}]"));
                        rest = &rest[at + len..];
                    } else {
                        break;
                    }
                }
                let held = values
                    .iter()
                    .flat_map(|(_, value)| {
                        value
                            .char_indices()
                            .skip(1)
                            .map(|(len, _)| len)
                            .filter(|len| *len <= rest.len() && rest.ends_with(&value[..*len]))
                    })
                    .max()
                    .unwrap_or(0);
                safe.push_str(&rest[..rest.len() - held]);
                notification["params"]["delta"] = json!(safe);
                if held > 0 {
                    if self.pending.len() >= 512 {
                        // fail closed under hostile stream fanout
                        notification["params"]["delta"] = json!(format!("{safe}[secret:withheld]"));
                    } else {
                        let mut template = notification.clone();
                        template["params"]["delta"] = json!("");
                        redact_notification(&mut template);
                        self.pending.insert(
                            key,
                            (template, Zeroizing::new(rest[rest.len() - held..].into())),
                        );
                    }
                }
            }
        } else if method.ends_with("/completed") || method.ends_with("/failed") || method == "error"
        {
            let keys: Vec<_> = self
                .pending
                .iter()
                .filter(|(_, (n, _))| {
                    n.get("_wonderRuntimeId").unwrap_or(&Value::Null) == &runtime
                        && n["params"].get("threadId").unwrap_or(&Value::Null) == &thread
                        && n["params"].get("turnId").unwrap_or(&Value::Null) == &turn
                        && (method.starts_with("turn/")
                            || item.is_null()
                            || n["params"]["itemId"] == item)
                })
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                if let Some((mut n, tail)) = self.pending.remove(&key) {
                    n["params"]["delta"] = json!(redact_text(&tail));
                    ready.push(n);
                }
            }
        }
        redact_notification(&mut notification);
        ready.push(notification);
        ready
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn secret_redaction_holds_only_ambiguous_suffixes_across_stream_chunks() {
        let value = format!("vault-{}-end", uuid::Uuid::new_v4());
        remember(&Record {
            info: SecretInfo {
                secret_ref: "fixture".into(),
                name: "STREAM_TOKEN".into(),
                project_id: "fixture".into(),
                uses: vec![],
            },
            value: value.clone(),
        });
        let mut redactor = NotificationRedactor::default();
        let event = |delta: &str, item: &str| json!({"_wonderRuntimeId":"fixture-stream-runtime","method":"item/commandExecution/outputDelta","params":{"threadId":"thread","turnId":"turn","itemId":item,"delta":delta}});
        let split = value.len() / 2;
        let first = redactor.ingest(event(&format!("before {}", &value[..split]), "one"));
        assert_eq!(first[0]["params"]["delta"], "before ");
        let second = redactor.ingest(event(&format!("{} after", &value[split..]), "one"));
        assert_eq!(second[0]["params"]["delta"], "[secret:STREAM_TOKEN] after");
        let mut serialized = first;
        serialized.extend(second);
        assert!(!serde_json::to_string(&serialized).unwrap().contains(&value));
        // Interleaved tools must not join fragments from different items.
        let a = redactor.ingest(event(&value[..split], "one"));
        let b = redactor.ingest(event("ordinary output", "two"));
        assert_eq!(a[0]["params"]["delta"], "");
        assert_eq!(b[0]["params"]["delta"], "ordinary output");
        let end = redactor.ingest(json!({"_wonderRuntimeId":"fixture-stream-runtime","method":"turn/completed","params":{"threadId":"thread","turnId":"turn"}}));
        assert_eq!(end.len(), 2);
        assert_eq!(end[0]["params"]["delta"], &value[..split]);
        assert!(redactor.pending.is_empty());
    }
}

/// Recover only this installation's abandoned delivery directories. A second
/// host must never remove a running host's files: owner PIDs are checked first.
#[cfg(unix)]
pub fn cleanup_delivery(data_dir: &std::path::Path) -> std::io::Result<()> {
    let root = data_dir.join("secret-delivery");
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(std::io::Error::other(
                "Secret delivery directory must not be a link.",
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    let entries = std::fs::read_dir(&root)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix("command-")
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        if unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            && entry.file_type()?.is_dir()
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
#[cfg(not(unix))]
pub fn cleanup_delivery(_: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}
