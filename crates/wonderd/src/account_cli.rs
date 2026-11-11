//! Read this computer's sign-in with the provider's own CLI.
use crate::AppState;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use wonder_store::AgentFamily;

/// A status command answers within this.
const CLI_TIMEOUT: Duration = Duration::from_secs(20);
/// The environment a provider CLI keeps; nothing else of Wonder's reaches it.
const KEPT: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    // Native Windows CLIs and credential stores resolve the profile, config
    // and temporary directories through these variables, rather than HOME.
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
];

/// What the provider's status command says about this computer's sign-in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CliSignIn {
    SignedIn {
        plan: Option<String>,
        auth_mode: Option<&'static str>,
    },
    /// Signed in to Claude without a plan Wonder can use.
    NeedsSubscription {
        plan: Option<String>,
    },
    SignedOut,
    /// The command could not answer; the detail, when there is one, says why.
    Unavailable(Option<String>),
}

/// The program and leading arguments that run a provider's own CLI, plus
/// the variables that point it at this computer's provider home.
struct Cli {
    program: PathBuf,
    leading: Vec<OsString>,
    env: Vec<(&'static str, OsString)>,
}

const CLAUDE_MISSING: &str = crate::claude::RUNTIME_MISSING;

fn cli(state: &AppState, family: AgentFamily, home: &Path) -> Result<Cli, String> {
    match family {
        // Wonder's bundled Claude Code, the one its runtime runs, through
        // the runtime's own manage script (`login`, `logout`, `status`).
        AgentFamily::Claude => {
            let bridge =
                crate::claude::bridge_runtime(state, family).ok_or(CLAUDE_MISSING.to_owned())?;
            Ok(Cli {
                program: bridge.config.node_bin.clone(),
                leading: vec![bridge
                    .config
                    .entrypoint
                    .with_file_name("manage.mjs")
                    .into_os_string()],
                env: vec![
                    ("CLAUDE_CONFIG_DIR", home.as_os_str().to_owned()),
                    ("WONDER_DATA_DIR", state.data_dir.as_os_str().to_owned()),
                ],
            })
        }
        AgentFamily::Codex => Ok(Cli {
            program: state.projects.launch_config().codex_bin,
            leading: vec!["login".into()],
            env: vec![("CODEX_HOME", home.as_os_str().to_owned())],
        }),
        AgentFamily::OpenCode | AgentFamily::Cursor => Err(format!(
            "{} has no accounts in Cupola.",
            family.provider().display_name
        )),
    }
}

/// Runs the CLI with `action` and returns (exit code, stdout, stderr).
async fn run(cli: &Cli, action: Option<&str>) -> Option<(i32, String, String)> {
    let mut command = tokio::process::Command::new(&cli.program);
    command
        .args(&cli.leading)
        .args(action)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let selection =
        std::env::var(wonder_app_server::provider_environment::SELECTION).unwrap_or_default();
    for (name, value) in std::env::vars_os() {
        let key = name.to_str().unwrap_or("");
        if KEPT.contains(&key)
            || wonder_app_server::provider_environment::runtime_variable(key, &selection)
        {
            command.env(name, value);
        }
    }
    for (name, value) in &cli.env {
        command.env(name, value);
    }
    let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    Some((
        output.status.code()?,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Signed-in state of the account in `home`, from the provider's own status
/// command. Starts no model work and no runtime.
pub(crate) async fn status(state: &AppState, family: AgentFamily, home: &Path) -> CliSignIn {
    let cli = match cli(state, family, home) {
        Ok(cli) => cli,
        Err(detail) => return CliSignIn::Unavailable(Some(detail)),
    };
    // Claude: `manage.mjs status` prints "version<TAB>plan" and exits 0 when
    // signed in, 42 signed out, 44 when the runtime is missing and 45 when
    // the sign-in has no Claude plan. Codex: `codex login status`.
    let Some((code, stdout, stderr)) = run(&cli, Some("status")).await else {
        return CliSignIn::Unavailable(None);
    };
    match family {
        AgentFamily::Claude => classify_claude(code, &stdout),
        _ => classify_codex(code, &format!("{stdout}\n{stderr}")),
    }
}

fn classify_claude(code: i32, stdout: &str) -> CliSignIn {
    let plan = stdout
        .lines()
        .last()
        .and_then(|line| line.split('\t').nth(1))
        .map(|plan| plan.trim().trim_start_matches("Claude ").to_lowercase())
        .filter(|plan| !plan.is_empty() && plan.len() <= 40);
    match code {
        0 => CliSignIn::SignedIn {
            auth_mode: Some(if plan.as_deref() == Some("api key") {
                "apiKey"
            } else {
                "claudeSubscription"
            }),
            plan,
        },
        42 => CliSignIn::SignedOut,
        44 => CliSignIn::Unavailable(Some(CLAUDE_MISSING.to_owned())),
        45 => CliSignIn::NeedsSubscription { plan },
        _ => CliSignIn::Unavailable(None),
    }
}

/// `codex login status` names the sign-in kind; its output is never passed
/// on, because an API-key sign-in prints part of the key.
fn classify_codex(code: i32, output: &str) -> CliSignIn {
    if code == 0 {
        let auth_mode = if output.contains("ChatGPT") {
            Some("chatgpt")
        } else if output.contains("API key") {
            Some("apiKey")
        } else {
            None
        };
        return CliSignIn::SignedIn {
            plan: None,
            auth_mode,
        };
    }
    if output.contains("Not logged in") {
        CliSignIn::SignedOut
    } else {
        CliSignIn::Unavailable(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_reads_exit_codes_and_the_plan_only() {
        assert_eq!(
            classify_claude(0, "2.1.288\tAPI key\n"),
            CliSignIn::SignedIn {
                plan: Some("api key".into()),
                auth_mode: Some("apiKey"),
            }
        );
        assert_eq!(
            classify_claude(0, "2.1.288\tClaude Max\n"),
            CliSignIn::SignedIn {
                plan: Some("max".into()),
                auth_mode: Some("claudeSubscription")
            }
        );
        assert_eq!(classify_claude(42, "2.1.288\t\n"), CliSignIn::SignedOut);
        assert_eq!(
            classify_claude(45, "2.1.288\tClaude Pro\n"),
            CliSignIn::NeedsSubscription {
                plan: Some("pro".into())
            }
        );
        assert!(matches!(
            classify_claude(44, ""),
            CliSignIn::Unavailable(Some(_))
        ));
        assert_eq!(classify_claude(1, ""), CliSignIn::Unavailable(None));
    }

    #[test]
    fn codex_status_names_the_kind_without_passing_output_on() {
        assert_eq!(
            classify_codex(0, "Logged in using ChatGPT\n"),
            CliSignIn::SignedIn {
                plan: None,
                auth_mode: Some("chatgpt")
            }
        );
        assert_eq!(
            classify_codex(0, "Logged in using an API key - sk-proj-***\n"),
            CliSignIn::SignedIn {
                plan: None,
                auth_mode: Some("apiKey")
            }
        );
        assert_eq!(classify_codex(1, "Not logged in\n"), CliSignIn::SignedOut);
        assert_eq!(classify_codex(1, "boom"), CliSignIn::Unavailable(None));
    }
}
