//! Host settings people set are named `CUPOLA_*`; the older `WONDER_*` names
//! still work. The host reads the older names internally, so each entry
//! point copies a `CUPOLA_*` value over its older name before anything else
//! reads the environment or starts a thread, and drops the `CUPOLA_*` name:
//! what a process passes its children (`cupola start --data-dir`) is then the
//! only value they see.

/// The settings that have a `CUPOLA_` name, without either prefix.
pub const NAMES: &[&str] = &[
    "DATA_DIR",
    "LOG_DIR",
    "CODEX_BIN",
    "CLAUDE_BRIDGE",
    "OPENCODE_BRIDGE",
    "OPENCODE_BIN",
    "NODE_BIN",
    "DAEMON_BIN",
    "BIN_DIR",
    "COMPUTER_PYTHON",
    "COMPUTER_BACKEND",
];

/// Makes each `CUPOLA_*` setting win over its `WONDER_*` alias. Call first in
/// `main`, while the process has one thread.
pub fn adopt() {
    for name in NAMES {
        let cupola = format!("CUPOLA_{name}");
        if let Some(value) = std::env::var_os(&cupola) {
            std::env::set_var(format!("WONDER_{name}"), value);
            std::env::remove_var(cupola);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    fn fixture(name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            &format!("cupola_env::tests::{name}"),
            "--nocapture",
        ]);
        command
    }

    /// `CUPOLA_DATA_DIR=A cupola start --data-dir B`: the launcher and the
    /// daemon it starts both use B.
    #[test]
    fn a_launcher_choice_reaches_its_children_over_the_shell_setting() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("seen");
        let status = fixture("launcher_fixture")
            .env("CUPOLA_ENV_FIXTURE", &output)
            .env("CUPOLA_DATA_DIR", "/A")
            .env("CUPOLA_LOG_DIR", "/A/logs")
            .env_remove("WONDER_DATA_DIR")
            .env_remove("WONDER_LOG_DIR")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "launcher /B\ndaemon /B /B/logs\n"
        );
    }

    #[test]
    fn launcher_fixture() {
        let Some(output) = std::env::var_os("CUPOLA_ENV_FIXTURE") else {
            return;
        };
        super::adopt();
        assert_eq!(std::env::var("WONDER_DATA_DIR").unwrap(), "/A");
        // As `cupola start --data-dir B` and `daemon_command` do.
        std::env::set_var("WONDER_DATA_DIR", "/B");
        let launcher = std::env::var("WONDER_DATA_DIR").unwrap();
        std::fs::write(&output, format!("launcher {launcher}\n")).unwrap();
        let status = fixture("daemon_fixture")
            .env("WONDER_DATA_DIR", "/B")
            .env("WONDER_LOG_DIR", "/B/logs")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn daemon_fixture() {
        let Some(output) = std::env::var_os("CUPOLA_ENV_FIXTURE") else {
            return;
        };
        super::adopt();
        let mut seen = std::fs::read_to_string(&output).unwrap();
        seen.push_str(&format!(
            "daemon {} {}\n",
            std::env::var("WONDER_DATA_DIR").unwrap(),
            std::env::var("WONDER_LOG_DIR").unwrap()
        ));
        std::fs::write(&output, seen).unwrap();
    }
}
