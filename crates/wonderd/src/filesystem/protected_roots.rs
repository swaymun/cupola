//! Credential locations Project file access must never open, including
//! folders moved with each tool's own setting.
//!
//! A Project folder that contains or sits inside one of these is refused
//! (`projects::validate_folder`), so entries stay narrow: a saved Project that
//! contains one stops working until it is edited.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// A folder named by a tool's setting, resolved the way this process opens it:
/// a relative value is taken from `cwd`. Empty values count as unset.
fn configured_folder(value: Option<OsString>, cwd: &Path) -> Option<PathBuf> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| cwd.join(value))
}

/// Claude Code's folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`. Both the host
/// and the protected list use this, so they never disagree about its location.
pub fn claude_config_home(
    home: &Path,
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> PathBuf {
    configured_folder(env("CLAUDE_CONFIG_DIR"), cwd).unwrap_or_else(|| home.join(".claude"))
}

/// Every protected location for this host.
pub fn sensitive_roots(
    home: &Path,
    cwd: &Path,
    codex_state: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<PathBuf> {
    // XDG requires an absolute value and says to ignore a relative one.
    let config_home = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let mut roots = vec![
        codex_state.to_path_buf(),
        home.join(".claude"),
        home.join(".claude.json"),
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".azure"),
        home.join(".gnupg"),
        home.join(".kube"),
        home.join(".netrc"),
        home.join(".git-credentials"),
        home.join(".npmrc"),
        home.join(".pypirc"),
        home.join(".docker"),
        home.join("Library/Keychains"),
        home.join(".config/gcloud"),
        home.join(".config/gh"),
        // The App Store Connect upload key; its tools never move it.
        home.join(".config/wonder"),
        home.join(".local/share/opencode/auth.json"),
    ];
    const CLAUDE: &[&str] = &[".credentials.json", ".claude.json", "settings.json"];
    const GH: &[&str] = &["hosts.yml"];
    const GCLOUD: &[&str] = &[
        "credentials.db",
        "access_tokens.db",
        "application_default_credentials.json",
        "legacy_credentials",
    ];
    const DOCKER: &[&str] = &["config.json"];
    let moved = [
        (Some(claude_config_home(home, cwd, env)), CLAUDE),
        (Some(config_home.join("gh")), GH),
        (configured_folder(env("GH_CONFIG_DIR"), cwd), GH),
        (Some(config_home.join("gcloud")), GCLOUD),
        (configured_folder(env("CLOUDSDK_CONFIG"), cwd), GCLOUD),
        (configured_folder(env("DOCKER_CONFIG"), cwd), DOCKER),
    ];
    let resolved_home = resolved(home);
    for (folder, sign_in_files) in moved {
        let Some(folder) = folder else { continue };
        // Protecting a folder that holds the home folder would refuse every
        // Project, so then only the tool's sign-in files are protected.
        if resolved_home.starts_with(resolved(&folder)) {
            roots.extend(sign_in_files.iter().map(|name| folder.join(name)));
        } else {
            roots.push(folder);
        }
    }
    roots.sort_unstable();
    roots.dedup();
    roots
}

/// macOS opens every path on the user's volume a second way: `/Users/me/x` is
/// also `/System/Volumes/Data/Users/me/x`. Canonicalizing keeps whichever
/// spelling it was given, so a protected folder must be listed under both.
const MACOS_DATA_FIRMLINK: &str = "/System/Volumes/Data";

/// `roots` plus each one under `prefix`, the second spelling of the same folder.
fn with_aliases_under(roots: Vec<String>, prefix: &str) -> Vec<String> {
    let mut all = roots.clone();
    all.extend(
        roots
            .iter()
            .filter(|root| root.starts_with('/') && !root.starts_with(&format!("{prefix}/")))
            .map(|root| format!("{prefix}{}", root.strip_suffix("/**").unwrap_or(root))),
    );
    all.sort_unstable();
    all.dedup();
    all
}

/// The denied folders every file check compares against: `roots` and, on macOS,
/// their `/System/Volumes/Data` spellings. Other systems have no second name.
pub fn with_firmlink_aliases(roots: Vec<String>) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return roots;
    }
    // Only the resolved spelling lies under the firmlink: `/var` is a link to
    // `/private/var`, and `/System/Volumes/Data/var` does not exist.
    let mut all = roots.clone();
    all.extend(roots.iter().filter_map(|root| {
        let root = root.strip_suffix("/**").unwrap_or(root);
        resolved(Path::new(root)).to_str().map(str::to_owned)
    }));
    with_aliases_under(all, MACOS_DATA_FIRMLINK)
}

/// Where a path really is, through links, as Project validation sees it.
fn resolved(path: &Path) -> PathBuf {
    super::protected_root(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{validate_execution_roots, validate_folders};
    use std::collections::HashMap;

    struct Host {
        _temp: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
    }

    impl Host {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let cwd = temp.path().join("service");
            for folder in [
                home.join("work/app"),
                home.join(".config/gh"),
                home.join(".docker/contexts"),
                home.join("Library/Keychains"),
                home.join("Library/Application Support/Notes"),
                home.join(".local/share/opencode"),
                cwd.clone(),
            ] {
                std::fs::create_dir_all(folder).unwrap();
            }
            Self {
                _temp: temp,
                home,
                cwd,
            }
        }

        fn denied(&self, env: &[(&str, &Path)]) -> Vec<String> {
            let env: HashMap<&str, OsString> = env
                .iter()
                .map(|(name, value)| (*name, value.as_os_str().to_owned()))
                .collect();
            sensitive_roots(&self.home, &self.cwd, &self.home.join(".codex"), &|name| {
                env.get(name).cloned()
            })
            .into_iter()
            .map(|path| path.to_str().unwrap().to_owned())
            .collect()
        }

        fn folder(&self, relative: &str) -> PathBuf {
            let path = self.home.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    fn allows(path: &Path, denied: &[String]) -> bool {
        validate_folders(&[path.to_str().unwrap().to_owned()], denied).is_ok()
    }

    // Contract: credential folders cannot be opened as a Project, nor can a
    // folder holding one, while ordinary folders beside them still can.
    #[test]
    fn protected_roots_refuse_credential_folders_and_allow_ordinary_ones() {
        let host = Host::new();
        let denied = host.denied(&[]);
        for refused in [
            ".config/gh",
            ".docker",
            ".docker/contexts",
            "Library/Keychains",
            "Library",
            ".local/share",
            ".config",
        ] {
            assert!(!allows(&host.folder(refused), &denied), "{refused}");
        }
        for allowed in ["work/app", "work", "Library/Application Support/Notes"] {
            assert!(allows(&host.folder(allowed), &denied), "{allowed}");
        }
    }

    // Contract: a Claude, GitHub CLI, gcloud or Docker folder moved with the
    // tool's own setting is protected, including a relative setting, which is
    // resolved from the host's working folder exactly as the host uses it.
    #[test]
    fn protected_roots_follow_moved_and_relative_tool_folders() {
        let host = Host::new();
        let relative = Path::new("moved-claude");
        let moved_claude = host.cwd.join(relative);
        std::fs::create_dir_all(&moved_claude).unwrap();
        std::fs::write(moved_claude.join(".credentials.json"), "{}").unwrap();
        let env_claude: HashMap<&str, OsString> =
            HashMap::from([("CLAUDE_CONFIG_DIR", relative.as_os_str().to_owned())]);
        assert_eq!(
            claude_config_home(&host.home, &host.cwd, &|name| env_claude.get(name).cloned()),
            moved_claude
        );
        let denied = host.denied(&[("CLAUDE_CONFIG_DIR", relative)]);
        assert!(!allows(&moved_claude, &denied));
        assert!(!allows(&host.cwd, &denied));
        assert!(allows(&host.folder("work/app"), &denied));

        let outside = host.home.parent().unwrap().join("tools");
        let gh = outside.join("gh");
        let docker = outside.join("docker");
        let gcloud = outside.join("gcloud");
        let config = outside.join("config");
        for folder in [
            &gh,
            &docker,
            &gcloud,
            &config.join("gh"),
            &outside.join("app"),
        ] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let denied = host.denied(&[
            ("GH_CONFIG_DIR", gh.as_path()),
            ("DOCKER_CONFIG", docker.as_path()),
            ("CLOUDSDK_CONFIG", gcloud.as_path()),
            ("XDG_CONFIG_HOME", config.as_path()),
        ]);
        for refused in [&gh, &docker, &gcloud, &config.join("gh")] {
            assert!(!allows(refused, &denied), "{}", refused.display());
        }
        assert!(allows(&outside.join("app"), &denied));
    }

    // Contract: every protected folder is also listed under its macOS firmlink
    // spelling, once, and the second spelling is never aliased again.
    #[test]
    fn protected_roots_gain_one_firmlink_spelling_each() {
        let roots = vec![
            "/Users/me/.cupola".to_owned(),
            "/Users/me/Library/Keychains".to_owned(),
        ];
        let aliased = with_aliases_under(roots.clone(), "/System/Volumes/Data");
        assert_eq!(aliased.len(), 4);
        for root in &roots {
            assert!(aliased.contains(root));
            assert!(aliased.contains(&format!("/System/Volumes/Data{root}")));
        }
        assert_eq!(
            with_aliases_under(aliased.clone(), "/System/Volumes/Data"),
            aliased
        );
    }

    // Contract: on macOS a Project folder spelled through the firmlink is
    // refused exactly like its ordinary spelling. Only macOS has the firmlink.
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_roots_refuse_the_macos_firmlink_spelling() {
        let host = Host::new();
        let home = crate::canonical_path::canonicalize(&host.home).unwrap();
        let sensitive = crate::canonical_path::canonicalize(host.folder(".config/gh")).unwrap();
        let alias = |path: &Path| format!("{MACOS_DATA_FIRMLINK}{}", path.display());
        let plain = host.denied(&[]);
        for refused in [sensitive.clone(), home.join("Library/Keychains")] {
            let spelled = alias(&refused);
            assert!(std::fs::metadata(&spelled).is_ok(), "{spelled}");
            assert!(
                validate_folders(std::slice::from_ref(&spelled), &plain).is_ok(),
                "without aliases the firmlink spelling slips through: {spelled}"
            );
            assert!(
                validate_folders(
                    std::slice::from_ref(&spelled),
                    &with_firmlink_aliases(plain.clone())
                )
                .is_err(),
                "{spelled}"
            );
        }
        let app = alias(&crate::canonical_path::canonicalize(host.folder("work/app")).unwrap());
        assert!(validate_folders(&[app], &with_firmlink_aliases(plain)).is_ok());
    }

    // Contract: a tool folder set to the home folder, one of its parents, or a
    // link to either protects only that tool's sign-in files, so ordinary
    // Projects keep working.
    #[cfg(unix)]
    #[test]
    fn protected_roots_keep_projects_usable_when_a_tool_folder_is_home() {
        let host = Host::new();
        let alias = host.home.parent().unwrap().join("home-link");
        std::os::unix::fs::symlink(&host.home, &alias).unwrap();
        let app = host.folder("work/app");
        let parent = host.home.parent().unwrap().to_path_buf();
        for setting in [host.home.as_path(), alias.as_path(), parent.as_path()] {
            for name in [
                "CLAUDE_CONFIG_DIR",
                "GH_CONFIG_DIR",
                "DOCKER_CONFIG",
                "CLOUDSDK_CONFIG",
            ] {
                let denied = host.denied(&[(name, setting)]);
                assert!(allows(&app, &denied), "{name}={}", setting.display());
                if name == "CLAUDE_CONFIG_DIR" {
                    assert!(
                        denied.contains(
                            &setting.join("settings.json").to_string_lossy().into_owned()
                        ),
                        "user settings can contain API keys even when the config folder is HOME"
                    );
                }
            }
        }
    }

    // Contract: a saved Project is checked again before it is used, so one
    // whose folder now holds a protected location stops working, and one in an
    // ordinary folder keeps working.
    #[test]
    fn protected_roots_apply_to_saved_projects() {
        let host = Host::new();
        let before: Vec<String> = Vec::new();
        let library = validate_folders(
            &[host.folder("Library").to_str().unwrap().to_owned()],
            &before,
        )
        .unwrap();
        let app = validate_folders(
            &[host.folder("work/app").to_str().unwrap().to_owned()],
            &before,
        )
        .unwrap();
        let saved = |root: &wonder_store::ProjectRootInput| wonder_store::StoredProject {
            id: "p".into(),
            name: "Project".into(),
            is_included: true,
            pin_order: None,
            primary_root_id: "r".into(),
            roots_revision: 1,
            last_family: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            last_used_at: None,
            auto_archive: None,
            last_models: Vec::new(),
            claude_trust: Vec::new(),
            roots: vec![wonder_store::StoredProjectRoot {
                id: "r".into(),
                path: root.path.clone(),
                canonical_path: root.canonical_path.clone(),
                ordinal: 0,
            }],
        };
        let denied = host.denied(&[]);
        assert!(validate_execution_roots(&saved(&library[0]), &denied).is_err());
        assert!(validate_execution_roots(&saved(&app[0]), &denied).is_ok());
    }
}
