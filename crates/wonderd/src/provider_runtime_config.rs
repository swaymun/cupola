//! Runtime settings per provider, like T3 Code's provider settings: the program
//! to run, launch arguments, environment variables and, for Codex, its home folder.
//! wonderd saves them, checks them in plain words and hands them
//! to `wonder_app_server::provider_environment`, which applies them
//! when a runtime starts, so a change reaches the next launched session.
//!
//! Secret environment values never touch the database or the phone: the host
//! keeps them in the login Keychain and reports only that one is set.
use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use wonder_app_server::provider_environment::{
    last_launch, resolve_binary, set_runtime_settings, split_arguments, variable_problem,
    RuntimeSettings,
};
use wonder_store::{AgentFamily, Store};
use zeroize::Zeroizing;

const MAX_VARIABLES: usize = 32;
const MAX_VALUE_BYTES: usize = 8 * 1024;

#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
struct Saved {
    binary: Option<String>,
    args: Vec<String>,
    env: Vec<SavedVariable>,
    home: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SavedVariable {
    name: String,
    secret: bool,
    /// Never set for a secret: its value is only in the Keychain.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    binary: Option<String>,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    env: Vec<RequestVariable>,
    home: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestVariable {
    name: String,
    #[serde(default)]
    secret: bool,
    /// A secret without a value keeps the value already in the Keychain.
    value: Option<String>,
}

fn refuse(message: impl Into<String>) -> Response {
    projects::error(StatusCode::BAD_REQUEST, message)
}

fn unreadable() -> Response {
    projects::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Cupola couldn’t read these settings. Try again.",
    )
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn supports_home(family: AgentFamily) -> bool {
    family == AgentFamily::Codex
}

/// One Keychain service for all provider variables on this installation.
fn service(namespace: &str) -> String {
    use sha2::{Digest, Sha256};
    let prefix = if cfg!(test) {
        "org.cupola.fixture-provider-env"
    } else {
        "org.cupola.provider-env.v1"
    };
    format!("{prefix}.{}", hex::encode(Sha256::digest(namespace)))
}

fn namespace(state: &AppState) -> String {
    if cfg!(test) {
        state.data_dir.to_string_lossy().into_owned()
    } else {
        state.host_installation_id.clone()
    }
}

fn account(family: AgentFamily, name: &str) -> String {
    format!("{}/{}", family.as_str(), name)
}

#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::os::macos::keychain::SecKeychain;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    const UNAVAILABLE: &str =
        "The login Keychain is unavailable. Unlock it on your computer and try again.";
    fn login() -> Result<SecKeychain, String> {
        let home = std::env::var_os("HOME").ok_or(UNAVAILABLE)?;
        SecKeychain::open(PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
            .map_err(|_| UNAVAILABLE.into())
    }
    pub(super) fn read(service: &str, account: &str) -> Result<Option<Zeroizing<String>>, String> {
        match login()?.find_generic_password(service, account) {
            Ok((bytes, _)) => Ok(String::from_utf8(bytes.to_vec()).ok().map(Zeroizing::new)),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => Err(UNAVAILABLE.into()),
        }
    }
    pub(super) fn write(service: &str, account: &str, value: &str) -> Result<(), String> {
        login()?
            .set_generic_password(service, account, value.as_bytes())
            .map_err(|_| UNAVAILABLE.into())
    }
    pub(super) fn delete(service: &str, account: &str) -> Result<(), String> {
        use security_framework::item::{ItemClass, ItemSearchOptions};
        match ItemSearchOptions::new()
            .keychains(&[login()?])
            .class(ItemClass::generic_password())
            .service(service)
            .account(account)
            .delete()
        {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(_) => Err(UNAVAILABLE.into()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    use zeroize::Zeroizing;
    const UNAVAILABLE: &str = "Secret values need a computer with a login Keychain.";
    pub(super) fn read(_: &str, _: &str) -> Result<Option<Zeroizing<String>>, String> {
        Ok(None)
    }
    pub(super) fn write(_: &str, _: &str, _: &str) -> Result<(), String> {
        Err(UNAVAILABLE.into())
    }
    pub(super) fn delete(_: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn expand_home(typed: &str) -> PathBuf {
    match typed.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(typed)),
        None => PathBuf::from(typed),
    }
}

fn check_home(typed: &str) -> Result<PathBuf, String> {
    let path = expand_home(typed.trim());
    if !path.is_absolute() {
        return Err(format!("{typed} isn't a full path. Start it with / or ~/."));
    }
    if !path.is_dir() {
        return Err(format!(
            "No folder at {}. Create it first, then save again.",
            path.display()
        ));
    }
    Ok(path)
}

fn parse(config: &str) -> Saved {
    serde_json::from_str(config).unwrap_or_default()
}

/// Reads secret values from the Keychain and builds what a launch applies.
/// A secret missing from the Keychain is left out and reported as unset.
fn settings_for(
    namespace: &str,
    family: AgentFamily,
    saved: &Saved,
) -> (RuntimeSettings, Vec<String>) {
    let service = service(namespace);
    let mut env = Vec::new();
    let mut missing = Vec::new();
    for variable in &saved.env {
        let value = if variable.secret {
            match keychain::read(&service, &account(family, &variable.name)) {
                Ok(Some(value)) => Some(value),
                _ => None,
            }
        } else {
            variable.value.clone().map(Zeroizing::new)
        };
        match value {
            Some(value) => env.push((variable.name.clone(), value.to_string())),
            None => missing.push(variable.name.clone()),
        }
    }
    let settings = RuntimeSettings {
        binary: saved.binary.as_deref().map(PathBuf::from),
        args: saved.args.clone(),
        env,
        home: saved.home.as_deref().map(PathBuf::from),
    };
    (settings, missing)
}

/// Loads every saved setting into the launch registry. Called at startup,
/// before the first runtime starts.
pub async fn hydrate(store: &Store, namespace: &str) -> Result<(), sqlx::Error> {
    for (family, config) in store.provider_runtime_configs().await? {
        let saved = parse(&config);
        let namespace = namespace.to_owned();
        if let Ok((settings, _)) =
            tokio::task::spawn_blocking(move || settings_for(&namespace, family, &saved)).await
        {
            set_runtime_settings(family.as_str(), Some(settings));
        }
    }
    Ok(())
}

/// Why the saved settings are not in effect yet, in the owner's words.
async fn notice(state: &AppState, family: AgentFamily, saved: &Saved) -> Option<String> {
    let name = family.provider().display_name;
    if supports_home(family)
        && saved.home.as_deref().map(std::path::Path::new)
            != wonder_app_server::provider_environment::active_codex_home()
    {
        return Some(format!(
            "Restart Cupola on your computer to use this home folder for {name}."
        ));
    }
    crate::provider_runtime::waiting_for_settings(state, family)
        .await
        .then(|| format!("{name} is still working. These apply when it finishes."))
}

async fn listing(state: &AppState, failures: &[(AgentFamily, String)]) -> Response {
    let Ok(configs) = state.store.provider_runtime_configs().await else {
        return unreadable();
    };
    let namespace = namespace(state);
    let mut providers = Vec::new();
    for family in AgentFamily::ALL {
        let saved = configs
            .iter()
            .find(|(f, _)| *f == family)
            .map(|(_, c)| parse(c))
            .unwrap_or_default();
        let (ns, saved_for_read) = (namespace.clone(), saved.clone());
        let missing =
            tokio::task::spawn_blocking(move || settings_for(&ns, family, &saved_for_read).1)
                .await
                .unwrap_or_default();
        let launch = last_launch(family.as_str());
        let notice = match failures.iter().find(|(f, _)| *f == family) {
            Some((_, detail)) => Some(format!(
                "Saved, but {} didn’t start with these: {detail}",
                family.provider().display_name
            )),
            None => notice(state, family, &saved).await,
        };
        providers.push(json!({
            "family": family.as_str(),
            "binary": saved.binary,
            "arguments": saved.args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
            // Secret values are never sent; the phone only learns one is set.
            "env": saved.env.iter().map(|v| json!({
                "name": v.name, "secret": v.secret, "value": v.value,
                "set": !missing.contains(&v.name),
            })).collect::<Vec<_>>(),
            "home": saved.home,
            "supportsHome": supports_home(family),
            "notice": notice,
            "lastLaunch": launch.map(|l| json!({
                "binary": l.binary, "arguments": l.args, "environmentNames": l.env_names, "home": l.home,
            })),
        }));
    }
    Json(json!({ "providers": providers })).into_response()
}

fn quote(argument: &str) -> String {
    if !argument.is_empty()
        && !argument.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
    {
        return argument.to_owned();
    }
    format!(
        "\"{}\"",
        argument.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    listing(&state, &[]).await
}

fn checked(family: AgentFamily, request: &Request, previous: &Saved) -> Result<Saved, String> {
    let binary = match request
        .binary
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(typed) => Some(resolve_binary(typed)?.display().to_string()),
        None => None,
    };
    let args = split_arguments(&request.arguments)?;
    let home = match request
        .home
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        Some(_) if !supports_home(family) => {
            return Err("Only Codex has a home folder setting.".into())
        }
        Some(typed) => Some(check_home(typed)?.display().to_string()),
        None => None,
    };
    if request.env.len() > MAX_VARIABLES {
        return Err(format!(
            "Cupola keeps up to {MAX_VARIABLES} variables per provider."
        ));
    }
    let mut env: Vec<SavedVariable> = Vec::new();
    for variable in &request.env {
        let name = variable.name.trim();
        if let Some(problem) = variable_problem(name) {
            return Err(problem);
        }
        if env.iter().any(|v| v.name == name) {
            return Err(format!("{name} is listed twice."));
        }
        if variable
            .value
            .as_ref()
            .is_some_and(|v| v.len() > MAX_VALUE_BYTES || v.contains('\0'))
        {
            return Err(format!(
                "The value of {name} is too long or has a null character."
            ));
        }
        if variable.secret {
            let kept = previous.env.iter().any(|v| v.name == name && v.secret);
            if variable.value.is_none() && !kept {
                return Err(format!("Enter a value for {name}."));
            }
            env.push(SavedVariable {
                name: name.into(),
                secret: true,
                value: None,
            });
        } else {
            let Some(value) = variable.value.clone() else {
                return Err(format!("Enter a value for {name}."));
            };
            env.push(SavedVariable {
                name: name.into(),
                secret: false,
                value: Some(value),
            });
        }
    }
    Ok(Saved {
        binary,
        args,
        env,
        home,
    })
}

pub(crate) async fn set(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Path(family): Path<String>,
    Json(request): Json<Request>,
) -> Response {
    let Some(family) = AgentFamily::ALL.into_iter().find(|f| f.as_str() == family) else {
        return projects::error(StatusCode::NOT_FOUND, "That provider isn’t available.");
    };
    let Ok(configs) = state.store.provider_runtime_configs().await else {
        return unreadable();
    };
    let previous = configs
        .iter()
        .find(|(f, _)| *f == family)
        .map(|(_, c)| parse(c))
        .unwrap_or_default();
    let saved = match checked(family, &request, &previous) {
        Ok(saved) => saved,
        Err(message) => return refuse(message),
    };
    let namespace = namespace(&state);
    // Secrets first: a Keychain failure undoes what it wrote and changes
    // nothing; a failure saving the rest puts the Keychain back as it was.
    let writes: Vec<_> = request
        .env
        .iter()
        .filter(|v| v.secret)
        .filter_map(|v| Some((v.name.trim().to_owned(), v.value.clone()?)))
        .collect();
    let removed: Vec<_> = previous
        .env
        .iter()
        .filter(|old| old.secret && !saved.env.iter().any(|v| v.name == old.name && v.secret))
        .map(|old| old.name.clone())
        .collect();
    let (ns, family_for_keychain) = (namespace.clone(), family);
    let keychain_result = tokio::task::spawn_blocking(move || {
        change_keychain(&service(&ns), family_for_keychain, writes, removed)
    })
    .await;
    let undo = match keychain_result {
        Ok(Ok(undo)) => undo,
        Ok(Err(message)) => {
            return projects::error(StatusCode::SERVICE_UNAVAILABLE, message);
        }
        Err(_) => return unreadable(),
    };
    let empty = saved == Saved::default();
    let config = serde_json::to_string(&saved).unwrap_or_default();
    if state
        .store
        .set_provider_runtime_config(family, (!empty).then_some(config.as_str()), &now())
        .await
        .is_err()
    {
        let ns = namespace.clone();
        let _ = tokio::task::spawn_blocking(move || {
            restore_keychain(&service(&ns), family, undo);
        })
        .await;
        return projects::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The setting could not be saved. Try again.",
        );
    }
    let ns = namespace.clone();
    let Ok((settings, _)) =
        tokio::task::spawn_blocking(move || settings_for(&ns, family, &saved)).await
    else {
        return unreadable();
    };
    set_runtime_settings(family.as_str(), (!empty).then_some(settings));
    // A runtime already running keeps what it started with; restart an idle
    // one now so the change reaches the next session.
    let mut failures = Vec::new();
    if let crate::provider_runtime::SettingsApplied::Failed(detail) =
        crate::provider_runtime::apply_runtime_settings(&state, family).await
    {
        failures.push((family, detail));
    }
    listing(&state, &failures).await
}

type KeychainUndo = Vec<(String, Option<Zeroizing<String>>)>;

/// Writes and removes secrets, remembering what each one held so a later
/// failure can put it back. A failure part-way restores what it already changed.
fn change_keychain(
    service: &str,
    family: AgentFamily,
    writes: Vec<(String, String)>,
    removed: Vec<String>,
) -> Result<KeychainUndo, String> {
    let mut undo: KeychainUndo = Vec::new();
    let result = (|| {
        for (name, value) in &writes {
            let account = account(family, name);
            undo.push((name.clone(), keychain::read(service, &account)?));
            keychain::write(service, &account, value)?;
        }
        for name in &removed {
            let account = account(family, name);
            undo.push((name.clone(), keychain::read(service, &account)?));
            keychain::delete(service, &account)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(undo),
        Err(message) => {
            restore_keychain(service, family, undo);
            Err(message)
        }
    }
}

fn restore_keychain(service: &str, family: AgentFamily, undo: KeychainUndo) {
    for (name, prior) in undo.into_iter().rev() {
        let account = account(family, &name);
        let _ = match prior {
            Some(value) => keychain::write(service, &account, &value),
            None => keychain::delete(service, &account),
        };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crate::test_support::{assert_http_contract, call, fixture};
    use axum::http::StatusCode;
    use serde_json::json;
    use wonder_app_server::provider_environment::{last_launch, runtime_settings};

    // Contract: a refused setting says why and saves nothing; an accepted one is
    // read back, reaches the launch registry, and a Cupola-owned name is refused.
    #[tokio::test]
    async fn runtime_settings_are_checked_saved_and_applied_to_the_next_launch() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, state) = fixture().await;
        let program = dir.path().join("claude-fixture");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let missing = dir.path().join("missing").display().to_string();
        let (status, refusal) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/cursor",
            json!({"binary": missing, "arguments": ""}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(refusal["error"]
            .as_str()
            .unwrap()
            .starts_with("Nothing is at "));
        let (_, none) = call(
            &state,
            "GET",
            "/api/v1/settings/provider-runtime",
            json!(null),
        )
        .await;
        assert_eq!(none["providers"][3]["binary"], json!(null));

        let (status, refusal) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/claude",
            json!({"env": [{"name": "CUPOLA_PROVIDER_ENV", "value": "x"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(refusal["error"]
            .as_str()
            .unwrap()
            .contains("can't be changed here"));
        let (status, _) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/claude",
            json!({"home": "/tmp"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "only Codex has a home folder"
        );

        let (status, saved) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/cursor",
            json!({
                "binary": program.display().to_string(),
                "arguments": "--flag \"two words\"",
                "env": [{"name": "CURSOR_API_BASE", "value": "https://gateway.example"}],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_http_contract("providerRuntimeSettings", &saved);
        let cursor = &saved["providers"][3];
        assert_eq!(cursor["binary"], json!(program.display().to_string()));
        assert_eq!(cursor["arguments"], json!("--flag \"two words\""));
        assert_eq!(cursor["env"][0]["value"], json!("https://gateway.example"));
        let applied = runtime_settings("cursor");
        assert_eq!(applied.args, ["--flag", "two words"]);
        assert_eq!(
            applied.env,
            [("CURSOR_API_BASE".into(), "https://gateway.example".into())]
        );

        // Clearing everything returns the provider to its defaults.
        let (_, cleared) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/cursor",
            json!({}),
        )
        .await;
        assert_eq!(cleared["providers"][3]["binary"], json!(null));
        assert_eq!(runtime_settings("cursor"), Default::default());
    }

    // Contract: saving settings restarts an idle running runtime so the next
    // session uses them (proved by what the new process was launched with, names
    // only), and a Codex home is held back for a restart because thread
    // identity and protected folders follow the home the host started with.
    #[tokio::test]
    async fn saving_restarts_an_idle_running_codex_and_holds_a_new_home_for_restart() {
        let (dir, state) = fixture().await;
        let home = dir.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let (status, saved) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/codex",
            json!({
                "env": [{"name": "UX10_PROBE", "value": "1"}],
                "home": home.display().to_string(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_http_contract("providerRuntimeSettings", &saved);
        let launch = last_launch("codex").expect("the idle runtime was restarted");
        assert_eq!(launch.env_names, ["UX10_PROBE"]);
        assert_eq!(launch.home, None, "the new home waits for a host restart");
        assert!(saved["providers"][0]["notice"]
            .as_str()
            .unwrap()
            .starts_with("Restart Cupola"));
        let (_, cleared) = call(
            &state,
            "PUT",
            "/api/v1/settings/provider-runtime/codex",
            json!({}),
        )
        .await;
        assert_eq!(cleared["providers"][0]["notice"], json!(null));
        assert_eq!(
            last_launch("codex").unwrap().env_names,
            Vec::<String>::new()
        );
    }
}
