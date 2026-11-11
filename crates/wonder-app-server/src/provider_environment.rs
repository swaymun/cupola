//! Explicit provider environment selection shared by service and bridge launch.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock, RwLock};

pub const SELECTION: &str = "CUPOLA_PROVIDER_ENV";

/// Windows preserves environment-key casing (usually `Path`, not `PATH`).
/// Keep OS process identity and executable lookup when clearing host secrets.
fn host_variable(name: &str) -> bool {
    [
        "HOME", "PATH", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL",
    ]
    .iter()
    .any(|known| known.eq_ignore_ascii_case(name))
        || windows_common(name)
}
/// The owner names exact variables, including arbitrary Codex `env_key` names.
/// Process identity, executable injection and Cupola control-plane settings
/// cannot be opted into through this provider-only channel.
pub fn selectable(name: &str) -> bool {
    if host_variable(name) {
        return false;
    }
    // A differently cased Windows key still controls the same child setting.
    let normalized = name.to_ascii_uppercase();
    let name = normalized.as_str();
    if name == "NODE_EXTRA_CA_CERTS" {
        return true;
    }
    let mut chars = name.bytes();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return false;
    }
    ![
        "WONDER_", "CUPOLA_", "LD_", "DYLD_", "NODE_", "PYTHON", "RUBY", "BASH", "LC_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        && !matches!(
            name,
            "TS_AUTHKEY" | "ENV" | "SHELLOPTS" | "ZDOTDIR" | "IFS" | "CDPATH" | "GCONV_PATH"
        )
}

pub fn selected(source: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let names = source.get(SELECTION).map(String::as_str).unwrap_or("");
    let mut result = BTreeMap::new();
    let names: Vec<_> = names
        .split(',')
        .map(str::trim)
        .filter(|name| selectable(name))
        .collect();
    for name in &names {
        if let Some(value) = source.get(*name) {
            result.insert((*name).to_owned(), value.clone());
        }
    }
    if !names.is_empty() {
        result.insert(SELECTION.into(), names.join(","));
    }
    result
}

/// Standard direct-shell settings plus explicitly selected custom credentials.
pub fn runtime_variable(name: &str, selection: &str) -> bool {
    matches!(
        name,
        "ANTHROPIC_API_KEY"
            | "ANTHROPIC_AUTH_TOKEN"
            | "ANTHROPIC_BASE_URL"
            | "CLAUDE_CODE_OAUTH_TOKEN"
            | "CLAUDE_CODE_GIT_BASH_PATH"
            | "CLAUDE_CODE_USE_POWERSHELL_TOOL"
            | "CLAUDE_CONFIG_DIR"
            | "OPENAI_API_KEY"
            | "OPENAI_BASE_URL"
            | "CURSOR_API_KEY"
            | "CURSOR_CONFIG_DIR"
            | "CURSOR_DATA_DIR"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
            | "no_proxy"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
            | "NODE_EXTRA_CA_CERTS"
    ) || name == SELECTION
        || (selectable(name) && selection.split(',').any(|item| item.trim() == name))
}

/// Variables a Windows program needs to start at all: Node aborts in its random
/// number setup without `SystemRoot`. Windows names are case-insensitive.
pub fn windows_common(name: &str) -> bool {
    const NAMES: [&str; 25] = [
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramData",
        "ALLUSERSPROFILE",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "TEMP",
        "TMP",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "OS",
        "PROCESSOR_ARCHITECTURE",
    ];
    NAMES.iter().any(|known| known.eq_ignore_ascii_case(name))
}

pub fn runtime_environment(
    family: &str,
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let variables: Vec<_> = variables.into_iter().collect();
    let selection = variables
        .iter()
        .find(|(key, _)| key == crate::provider_environment::SELECTION)
        .and_then(|(_, value)| value.to_str())
        .unwrap_or("")
        .to_owned();
    let mut environment = Vec::new();
    for (key, value) in variables {
        let name = key.to_str().unwrap_or("");
        let common = matches!(
            name,
            "HOME" | "PATH" | "USER" | "LOGNAME" | "SHELL" | "TMPDIR" | "LANG" | "LC_ALL"
        ) || (cfg!(windows)
            && (windows_common(name) || name.eq_ignore_ascii_case("PATH")));
        // Preserve the owner's native policy and storage paths. Arbitrary
        // credential interpolation cannot safely inherit Wonder's environment.
        let native = family == "opencode"
            && matches!(
                name,
                "WONDER_OPENCODE_BIN"
                    | "OPENCODE_CONFIG"
                    | "OPENCODE_CONFIG_DIR"
                    | "OPENCODE_CONFIG_CONTENT"
                    | "OPENCODE_PERMISSION"
                    | "OPENCODE_DISABLE_PROJECT_CONFIG"
                    | "OPENCODE_DB"
                    | "XDG_CONFIG_HOME"
                    | "XDG_DATA_HOME"
                    | "XDG_CACHE_HOME"
                    | "XDG_STATE_HOME"
            );
        if common
            || native
            || (family == "cursor" && name == "WONDER_CURSOR_BIN")
            || crate::provider_environment::runtime_variable(name, &selection)
        {
            environment.push((key, value));
        }
    }
    environment
}

/// What the owner chose for one provider's runtime on this computer. Applied
/// when a runtime starts, so a change reaches the next launched session; a
/// running one keeps what it started with.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RuntimeSettings {
    /// The program to run instead of the one Cupola finds.
    pub binary: Option<PathBuf>,
    /// Extra command-line arguments, one string each.
    pub args: Vec<String>,
    /// Extra environment variables. Secret values are read from the Keychain
    /// by the host and held only here, in memory.
    pub env: Vec<(String, String)>,
    /// Codex only: the Codex home (`CODEX_HOME`). The host reads it once, at
    /// startup, because thread identity and protected folders follow the home;
    /// launches use `active_codex_home`, never this field.
    pub home: Option<PathBuf>,
}

impl std::fmt::Debug for RuntimeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSettings")
            .field("binary", &self.binary)
            .field("args", &self.args)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("home", &self.home)
            .finish()
    }
}

/// What a runtime was last started with. Names only: values (secret or not)
/// never appear here, so a Diagnostics read cannot leak them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRecord {
    pub binary: Option<String>,
    pub args: Vec<String>,
    pub env_names: Vec<String>,
    pub home: Option<String>,
    /// The settings revision this launch used; see `settings_outdated`.
    pub revision: u64,
}

static SETTINGS: LazyLock<RwLock<HashMap<String, (u64, RuntimeSettings)>>> =
    LazyLock::new(Default::default);
static LAUNCHES: LazyLock<RwLock<HashMap<String, LaunchRecord>>> = LazyLock::new(Default::default);
static ACTIVE_CODEX_HOME: OnceLock<PathBuf> = OnceLock::new();

/// The host replaces a provider's settings when the owner saves them and at
/// startup. `None` returns that provider to its defaults. Every call is a new
/// revision, which is how a runtime started earlier is known to be out of date.
pub fn set_runtime_settings(family: &str, settings: Option<RuntimeSettings>) {
    let mut all = SETTINGS.write().unwrap_or_else(|e| e.into_inner());
    let revision = all.get(family).map_or(0, |(revision, _)| *revision) + 1;
    match settings {
        Some(settings) => all.insert(family.to_owned(), (revision, settings)),
        None => all.insert(family.to_owned(), (revision, RuntimeSettings::default())),
    };
}

pub fn runtime_settings(family: &str) -> RuntimeSettings {
    launch_settings(family).1
}

/// The settings and the revision they are, read together; a launch records
/// that revision so a later save is noticed.
pub fn launch_settings(family: &str) -> (u64, RuntimeSettings) {
    SETTINGS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(family)
        .cloned()
        .unwrap_or_default()
}

/// The Codex home this host started with. Set once at startup from the
/// owner's setting; every Codex launch uses it so the runtime, the thread
/// store identity and the protected folders always name the same home.
pub fn set_active_codex_home(home: PathBuf) {
    let _ = ACTIVE_CODEX_HOME.set(home);
}

pub fn active_codex_home() -> Option<&'static Path> {
    ACTIVE_CODEX_HOME.get().map(PathBuf::as_path)
}

/// What a launch applies from `settings`, recorded once the process started.
pub fn record_launch(family: &str, revision: u64, settings: &RuntimeSettings, home: Option<&Path>) {
    let record = LaunchRecord {
        binary: settings.binary.as_ref().map(|p| p.display().to_string()),
        args: settings.args.clone(),
        env_names: settings_environment(settings)
            .map(|(name, _)| name.to_owned())
            .collect(),
        home: home.map(|p| p.display().to_string()),
        revision,
    };
    let mut launches = LAUNCHES.write().unwrap_or_else(|e| e.into_inner());
    // Two launches can finish out of order; the newer settings stay on record.
    if launches
        .get(family)
        .is_some_and(|known| known.revision > revision)
    {
        return;
    }
    launches.insert(family.to_owned(), record);
}

pub fn last_launch(family: &str) -> Option<LaunchRecord> {
    LAUNCHES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(family)
        .cloned()
}

/// Whether the owner saved settings after this provider's runtime started, so
/// the runtime that is running still has the old ones.
pub fn settings_outdated(family: &str) -> bool {
    last_launch(family).is_some_and(|launch| launch.revision != launch_settings(family).0)
}

/// The selection a bridge gets: the names it inherited from the service launch
/// plus the owner's, each once.
pub fn merge_selection(inherited: Option<&str>, owner_names: &[&str]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for name in inherited
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .chain(owner_names.iter().copied())
    {
        if selectable(name) && !names.contains(&name) {
            names.push(name);
        }
    }
    names.join(",")
}

/// Variables from the owner's settings that may reach a provider process.
pub fn settings_environment(settings: &RuntimeSettings) -> impl Iterator<Item = (&str, &str)> {
    settings
        .env
        .iter()
        .filter(|(name, _)| selectable(name))
        .map(|(name, value)| (name.as_str(), value.as_str()))
}

/// Resolves what the owner typed to a program, or says plainly why not.
/// `~/` means the home folder; a bare name is looked up on PATH.
pub fn resolve_binary(typed: &str) -> Result<PathBuf, String> {
    let typed = typed.trim();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = if let Some(rest) = typed.strip_prefix("~/") {
        home.map(|home| home.join(rest))
            .ok_or_else(|| "This computer has no home folder to expand ~ from.".to_owned())?
    } else if typed.contains(['/', '\\']) {
        PathBuf::from(typed)
    } else if typed.is_empty() {
        return Err("Enter the path to the program.".into());
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(typed))
                    .find(|candidate| candidate.is_file())
            })
            .ok_or_else(|| format!("No program named {typed} was found on this computer's PATH."))?
    };
    if !path.is_absolute() {
        return Err(format!("{typed} isn't a full path. Start it with / or ~/."));
    }
    let meta =
        std::fs::metadata(&path).map_err(|_| format!("Nothing is at {}.", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder, not a program.", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{} can't be run. Make it executable (chmod +x).",
                path.display()
            ));
        }
    }
    Ok(path)
}

/// Splits launch arguments the way a shell would: spaces separate, quotes and
/// backslashes group. Anything unbalanced is refused, not guessed at.
pub fn split_arguments(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                current.push(
                    chars
                        .next()
                        .ok_or("An argument ends with a lone backslash.")?,
                );
                started = true;
            }
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("A quote in the arguments is never closed.".into());
    }
    if started {
        args.push(current);
    }
    Ok(args)
}

/// Why this variable name can't be set here, in the owner's words.
pub fn variable_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("A variable needs a name.".into());
    }
    if !selectable(name) {
        return Some(format!(
            "{name} is set by Cupola or the system and can't be changed here."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_runtime_keeps_the_variables_a_program_needs_to_start() {
        for name in [
            "CLAUDE_CODE_GIT_BASH_PATH",
            "CLAUDE_CODE_USE_POWERSHELL_TOOL",
        ] {
            let environment = runtime_environment("claude", [(name.into(), "owner-value".into())]);
            assert_eq!(environment, vec![(name.into(), "owner-value".into())]);
        }
        for name in [
            "SystemRoot",
            "SYSTEMROOT",
            "windir",
            "ComSpec",
            "APPDATA",
            "TEMP",
            "ProgramFiles(x86)",
        ] {
            assert!(super::windows_common(name), "{name}");
        }
        for name in [
            "TS_AUTHKEY",
            "WONDER_PUBLIC_ORIGIN",
            "OPENAI_API_KEY",
            "NODE_OPTIONS",
        ] {
            assert!(!super::windows_common(name), "{name}");
        }
    }

    #[test]
    fn windows_host_identity_cannot_be_selected_as_provider_credentials() {
        for name in [
            "Path",
            "COMSPEC",
            "SystemRoot",
            "USERPROFILE",
            "APPDATA",
            "TEMP",
            "PATHEXT",
        ] {
            assert!(!selectable(name), "{name} is host identity");
        }
        for name in [
            "node_options",
            "wonder_loopback_capability",
            "cupola_provider_env",
        ] {
            assert!(!selectable(name), "{name} cannot be injected by selection");
        }
    }
    #[test]
    fn launch_arguments_split_like_a_shell_and_refuse_unbalanced_quotes() {
        assert_eq!(
            split_arguments(r#"--model "claude opus" -c 'a b' plain\ one"#).unwrap(),
            ["--model", "claude opus", "-c", "a b", "plain one"]
        );
        assert_eq!(split_arguments("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_arguments(r#"--flag """#).unwrap(), ["--flag", ""]);
        assert!(split_arguments("--x \"open").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_cannot_run_is_refused_in_plain_words() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        let missing = resolve_binary(&format!("{}/nope", dir.path().display())).unwrap_err();
        assert!(missing.starts_with("Nothing is at "), "{missing}");
        let folder = resolve_binary(&dir.path().display().to_string()).unwrap_err();
        assert!(folder.contains("is a folder"), "{folder}");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
        let locked = resolve_binary(&program.display().to_string()).unwrap_err();
        assert!(locked.contains("chmod +x"), "{locked}");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_binary(&program.display().to_string()).unwrap(),
            program
        );
        assert!(resolve_binary("").is_err());
        assert!(resolve_binary("definitely-not-a-program-ux10")
            .unwrap_err()
            .contains("PATH"));
    }

    #[test]
    fn owner_variables_cannot_reach_what_cupola_owns_and_debug_hides_values() {
        assert!(variable_problem("ANTHROPIC_BASE_URL").is_none());
        assert!(variable_problem("CODEX_HOME").is_none());
        for name in [
            "CUPOLA_PROVIDER_ENV",
            "WONDER_CODEX_BIN",
            "NODE_OPTIONS",
            "PATH",
            "LD_PRELOAD",
            "",
        ] {
            assert!(variable_problem(name).is_some(), "{name}");
        }
        let settings = RuntimeSettings {
            env: vec![("ANTHROPIC_API_KEY".into(), "sk-secret-value".into())],
            ..Default::default()
        };
        let shown = format!("{settings:?}");
        assert!(
            shown.contains("ANTHROPIC_API_KEY") && !shown.contains("sk-secret-value"),
            "{shown}"
        );
        let recorded = {
            record_launch("ux10-test", 1, &settings, None);
            last_launch("ux10-test").unwrap()
        };
        assert_eq!(recorded.env_names, ["ANTHROPIC_API_KEY"]);
        assert!(!format!("{recorded:?}").contains("sk-secret-value"));
    }

    #[test]
    fn owner_names_join_the_inherited_selection_and_stale_launches_are_noticed() {
        assert_eq!(
            merge_selection(
                Some("MY_GATEWAY_KEY, PATH"),
                &["ANTHROPIC_BASE_URL", "MY_GATEWAY_KEY"]
            ),
            "MY_GATEWAY_KEY,ANTHROPIC_BASE_URL"
        );
        assert_eq!(merge_selection(None, &[]), "");
        set_runtime_settings("ux10-stale", None);
        let (revision, settings) = launch_settings("ux10-stale");
        record_launch("ux10-stale", revision, &settings, None);
        assert!(!settings_outdated("ux10-stale"));
        set_runtime_settings("ux10-stale", Some(RuntimeSettings::default()));
        assert!(settings_outdated("ux10-stale"));
    }

    #[test]
    fn saved_settings_replace_and_clear_per_provider() {
        let settings = RuntimeSettings {
            args: vec!["--x".into()],
            ..Default::default()
        };
        set_runtime_settings("ux10-family", Some(settings.clone()));
        assert_eq!(runtime_settings("ux10-family"), settings);
        set_runtime_settings("ux10-family", None);
        assert_eq!(runtime_settings("ux10-family"), RuntimeSettings::default());
    }

    #[test]
    fn an_api_key_reaches_the_runtime_environment_and_never_the_records() {
        let key = "sk-fixture-api-key-0000";
        for (family, name) in [("claude", "ANTHROPIC_API_KEY"), ("codex", "OPENAI_API_KEY")] {
            let owner = format!("apikey-{family}");
            let settings = RuntimeSettings {
                env: vec![(name.into(), key.into())],
                ..Default::default()
            };
            set_runtime_settings(&owner, Some(settings));
            let (revision, settings) = launch_settings(&owner);
            // The launch applies the saved key as an ordinary environment pair...
            let pairs: Vec<_> = settings_environment(&settings).collect();
            assert_eq!(pairs, [(name, key)]);
            // ...and the process environment keeps it for this provider.
            let launched = runtime_environment(
                family,
                pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())),
            );
            assert_eq!(launched, vec![(name.into(), key.into())]);
            // Debug output and the launch record carry the name, never the value.
            record_launch(&owner, revision, &settings, None);
            let record = last_launch(&owner).unwrap();
            assert_eq!(record.env_names, [name]);
            assert!(!format!("{settings:?}{record:?}").contains(key));
            // Removing the key returns to the login: nothing is applied.
            set_runtime_settings(&owner, None);
            assert_eq!(settings_environment(&launch_settings(&owner).1).count(), 0);
        }
    }
}
