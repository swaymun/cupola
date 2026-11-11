//! Hermetic Git for tests. A test that runs `git -C <tempdir>` still inherits
//! `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and the rest from the process
//! that started it (a Git hook, a worker session), and Git then works on that
//! repository instead of the tempdir. Fixtures that write hostile settings
//! (`core.bare`, `remote.origin.uploadpack`, filters) would land in a real
//! checkout's config. Every test spawn of Git goes through [`git`] or
//! [`hermetic`].

use std::ffi::OsString;
use std::process::Command;

/// Removes every inherited `GIT_*` variable (repository selection, config
/// injection, object storage, transport programs) and pins the config files,
/// so the command sees only its `-C` folder and that folder's own settings.
pub(crate) fn hermetic(command: &mut Command) -> &mut Command {
    let inherited: Vec<OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("GIT_"))
        .collect();
    for key in inherited {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
}

/// `git` with a hermetic environment.
pub(crate) fn git() -> Command {
    let mut command = Command::new("git");
    hermetic(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: &str = "WONDER_TEST_GIT_CHILD";

    /// Runs in a child copy of this test binary whose environment names a
    /// foreign repository, the way a Git hook or worker session would.
    #[test]
    fn child_runs_helpers_with_an_inherited_repository() {
        let Some(target) = std::env::var_os(CHILD) else {
            return;
        };
        let target = std::path::PathBuf::from(target);
        assert!(
            std::env::var_os("GIT_DIR").is_some(),
            "child must inherit GIT_DIR"
        );
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Cupola Test"],
            &["config", "core.alternateRefsCommand", "touch ran"],
        ] {
            let status = git().arg("-C").arg(&target).args(args).status().unwrap();
            assert!(status.success(), "git {args:?}");
        }
    }

    /// A test helper works on its `-C` folder whatever repository, index,
    /// work tree or injected config the process inherited.
    #[test]
    fn test_git_ignores_an_inherited_git_dir() {
        if std::env::var_os(CHILD).is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("foreign");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        assert!(git()
            .arg("init")
            .arg("-q")
            .arg(&foreign)
            .status()
            .unwrap()
            .success());
        let before = std::fs::read(foreign.join(".git/config")).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_git::tests::child_runs_helpers_with_an_inherited_repository",
                "--nocapture",
            ])
            .env(CHILD, &target)
            .env("GIT_DIR", foreign.join(".git"))
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_INDEX_FILE", foreign.join(".git/index"))
            .env("GIT_COMMON_DIR", foreign.join(".git"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.bare")
            .env("GIT_CONFIG_VALUE_0", "true")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let config = std::fs::read_to_string(target.join(".git/config")).unwrap();
        assert!(config.contains("alternateRefsCommand"), "{config}");
        assert!(config.contains("bare = false"), "{config}");
        assert_eq!(std::fs::read(foreign.join(".git/config")).unwrap(), before);
    }
}
