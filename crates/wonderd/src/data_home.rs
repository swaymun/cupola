//! Where Cupola keeps its data on macOS and Linux: `~/.cupola`.
//!
//! Older releases used `~/.wonder` (and before that the Mac app's
//! `~/Library/Application Support/Wonder`). The host moves an old directory
//! once, when it starts with no host running from it, and leaves the old name
//! as a link so saved absolute paths and the previous release keep working.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// The data directory under the home directory.
pub const DATA_DIR: &str = ".cupola";
/// The data directory of releases before Cupola; a link after the move.
pub const LEGACY_DATA_DIR: &str = ".wonder";
/// Left in the data directory between moving the Mac app's directory and
/// linking its old name; `prepare` finishes that link.
const MOVE_MARKER: &str = ".legacy-home-migration";
/// Left in the data directory between moving `~/.wonder` and linking its old
/// name. Separate from `MOVE_MARKER` so neither move clears the other's.
const WONDER_MOVE_MARKER: &str = ".wonder-home-move";

/// The default data directory, changing nothing: `~/.cupola`, or `~/.wonder`
/// while that still holds the data.
pub fn default_dir(home: &Path) -> PathBuf {
    if legacy_holds_data(home) {
        home.join(LEGACY_DATA_DIR)
    } else {
        home.join(DATA_DIR)
    }
}

/// True while `~/.wonder` exists and is not the link to `~/.cupola`.
fn legacy_holds_data(home: &Path) -> bool {
    let legacy = home.join(LEGACY_DATA_DIR);
    let Ok(metadata) = fs::symlink_metadata(&legacy) else {
        return false;
    };
    !(metadata.is_symlink()
        && crate::canonical_path::canonicalize(&legacy).ok()
            == crate::canonical_path::canonicalize(home.join(DATA_DIR)).ok())
}

/// `dir`, or `~/.cupola` when `dir` is `~/.wonder` and that is the link to
/// it, so both names resolve to one directory.
pub fn through_link(home: &Path, dir: &Path) -> PathBuf {
    let legacy = home.join(LEGACY_DATA_DIR);
    if dir == legacy && legacy.is_symlink() && !legacy_holds_data(home) {
        home.join(DATA_DIR)
    } else {
        dir.to_path_buf()
    }
}

/// The Keychain account naming a data directory's items. The default
/// directory keeps the name its items were saved under, `~/.wonder`, so the
/// move loses no saved key.
pub fn keychain_account(data_dir: &Path) -> String {
    account_in(
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        data_dir,
    )
}

fn account_in(home: Option<&Path>, data_dir: &Path) -> String {
    match home {
        Some(home) if data_dir == home.join(DATA_DIR) => {
            home.join(LEGACY_DATA_DIR).to_string_lossy().into_owned()
        }
        _ => data_dir.to_string_lossy().into_owned(),
    }
}

/// Moves `~/.wonder` to `~/.cupola` and links the old name to it. Returns the
/// directory to use: still `~/.wonder` while a host runs from it, so the move
/// happens at the first start with the host stopped.
pub fn move_legacy(home: &Path) -> io::Result<PathBuf> {
    let target = home.join(DATA_DIR);
    let legacy = home.join(LEGACY_DATA_DIR);
    // Recover a crash between the move and the link.
    if fs::symlink_metadata(&legacy).is_err() && target.join(WONDER_MOVE_MARKER).exists() {
        std::os::unix::fs::symlink(&target, &legacy)?;
        fs::remove_file(target.join(WONDER_MOVE_MARKER))?;
    }
    if !legacy_holds_data(home) {
        return Ok(target);
    }
    if fs::symlink_metadata(&target).is_ok() && fs::remove_dir(&target).is_err() {
        return Err(io::Error::other(
            "Both ~/.wonder and ~/.cupola hold Cupola data. Neither was changed; keep one of them, then start Cupola again.",
        ));
    }
    // A supervisor holds this lock while it runs, through either name.
    fs::create_dir_all(legacy.join("headless"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(legacy.join("headless/supervisor.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(legacy);
    }
    if in_use(&legacy.join("wonder.sqlite3"))? {
        return Err(io::Error::other(
            "A Cupola daemon is still using ~/.wonder. Stop it, then start Cupola again to move its data to ~/.cupola.",
        ));
    }
    fs::write(legacy.join(WONDER_MOVE_MARKER), b"")?;
    fs::rename(&legacy, &target)?;
    if let Err(error) = std::os::unix::fs::symlink(&target, &legacy) {
        fs::rename(&target, &legacy)?;
        let _ = fs::remove_file(legacy.join(WONDER_MOVE_MARKER));
        return Err(error);
    }
    fs::remove_file(target.join(WONDER_MOVE_MARKER))?;
    Ok(target)
}

/// Whether a process outside a supervisor (a daemon started by hand) has
/// the database open. Only macOS ships `lsof` everywhere.
fn in_use(database: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    if database.exists() {
        let result = std::process::Command::new("/usr/sbin/lsof")
            .args(["-t", "--"])
            .arg(database)
            .output()?;
        if result.status.success() {
            return Ok(true);
        }
        if result.status.code() != Some(1) {
            return Err(io::Error::other(
                "Could not check whether Cupola data is in use.",
            ));
        }
    }
    let _ = database;
    Ok(false)
}

/// The daemon's own default when no supervisor names a directory: the
/// default directory, after moving the Mac app's original one into it.
pub fn prepare(home: &Path) -> io::Result<PathBuf> {
    let target = default_dir(home);
    let legacy = home.join("Library/Application Support/Wonder");
    if legacy.is_symlink() {
        if crate::canonical_path::canonicalize(&legacy)?
            == crate::canonical_path::canonicalize(&target)?
        {
            return Ok(target);
        }
        return Err(io::Error::other(
            "The legacy data directory points elsewhere; move it manually before starting Cupola.",
        ));
    }
    if !legacy.exists() {
        // Recover a crash between moving the directory and installing its alias.
        if target.join(MOVE_MARKER).exists() {
            std::os::unix::fs::symlink(&target, &legacy)?;
            fs::remove_file(target.join(MOVE_MARKER))?;
        }
        fs::create_dir_all(&target)?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        }
        return Ok(target);
    }
    if target.exists() {
        return Err(io::Error::other(format!("Both {} and the legacy data directory exist. Neither was changed; reconcile them before starting Cupola.", target.display())));
    }
    fs::create_dir_all(legacy.join("Service"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(legacy.join("Service/launcher.pid"))?;
    lock.try_lock()
        .map_err(|_| io::Error::other("Quit the running Cupola app before migrating its data."))?;
    // A manually launched daemon may not have a supervisor lock.
    if in_use(&legacy.join("wonder.sqlite3"))? {
        return Err(io::Error::other(
            "Stop the running Cupola daemon before migrating its data.",
        ));
    }
    fs::write(legacy.join(MOVE_MARKER), b"")?;
    fs::rename(&legacy, &target)?;
    if let Err(error) = std::os::unix::fs::symlink(&target, &legacy) {
        fs::rename(&target, &legacy)?;
        return Err(error);
    }
    fs::remove_file(target.join(MOVE_MARKER))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wonder_home() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".wonder");
        fs::create_dir_all(legacy.join("headless")).unwrap();
        fs::write(legacy.join("host-key"), "key").unwrap();
        fs::write(legacy.join("host-installation-id"), "id\n").unwrap();
        fs::write(legacy.join("headless/config.json"), "{}").unwrap();
        (home, legacy)
    }

    #[test]
    fn a_fresh_home_uses_cupola() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(default_dir(home.path()), home.path().join(".cupola"));
        assert_eq!(
            move_legacy(home.path()).unwrap(),
            home.path().join(".cupola")
        );
        assert!(!home.path().join(".wonder").exists());
    }

    #[test]
    fn the_move_keeps_every_file_and_links_the_old_name() {
        let (home, legacy) = wonder_home();
        assert_eq!(default_dir(home.path()), legacy);
        let target = move_legacy(home.path()).unwrap();
        assert_eq!(target, home.path().join(".cupola"));
        assert!(fs::symlink_metadata(&target).unwrap().is_dir());
        assert!(legacy.is_symlink());
        assert_eq!(fs::read_to_string(target.join("host-key")).unwrap(), "key");
        // The previous release finds the same files under the old name.
        assert_eq!(
            fs::read_to_string(legacy.join("host-installation-id")).unwrap(),
            "id\n"
        );
        assert!(!target.join(WONDER_MOVE_MARKER).exists());
        assert_eq!(default_dir(home.path()), target);
        assert_eq!(move_legacy(home.path()).unwrap(), target);
    }

    #[test]
    fn a_running_host_keeps_its_directory_until_it_stops() {
        let (home, legacy) = wonder_home();
        let host = fs::File::create(legacy.join("headless/supervisor.lock")).unwrap();
        host.lock().unwrap();
        assert_eq!(move_legacy(home.path()).unwrap(), legacy);
        assert!(!legacy.is_symlink());
        assert!(!home.path().join(".cupola").exists());
        drop(host);
        assert_eq!(
            move_legacy(home.path()).unwrap(),
            home.path().join(".cupola")
        );
    }

    #[test]
    fn an_interrupted_move_finishes_its_link() {
        let (home, legacy) = wonder_home();
        fs::write(legacy.join(WONDER_MOVE_MARKER), "").unwrap();
        fs::rename(&legacy, home.path().join(".cupola")).unwrap();
        assert_eq!(default_dir(home.path()), home.path().join(".cupola"));
        move_legacy(home.path()).unwrap();
        assert!(legacy.is_symlink());
        assert!(!home
            .path()
            .join(".cupola")
            .join(WONDER_MOVE_MARKER)
            .exists());
    }

    #[test]
    fn an_interrupted_mac_app_move_survives_the_wonder_move() {
        // An older release moved the Mac app's directory to ~/.wonder and
        // stopped before linking the old name.
        let (home, legacy) = wonder_home();
        let old = home.path().join("Library/Application Support/Wonder");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(legacy.join(MOVE_MARKER), "").unwrap();
        let target = move_legacy(home.path()).unwrap();
        assert!(legacy.is_symlink());
        assert!(target.join(MOVE_MARKER).exists());
        assert!(!target.join(WONDER_MOVE_MARKER).exists());
        // The daemon then finishes the older link, to the new directory.
        assert_eq!(prepare(home.path()).unwrap(), target);
        assert!(old.is_symlink());
        assert_eq!(fs::read_to_string(old.join("host-key")).unwrap(), "key");
        assert!(!target.join(MOVE_MARKER).exists());
    }

    #[test]
    fn two_data_directories_are_never_merged() {
        let (home, legacy) = wonder_home();
        // An empty one is a leftover and gives way.
        fs::create_dir(home.path().join(".cupola")).unwrap();
        move_legacy(home.path()).unwrap();
        assert!(legacy.is_symlink());

        let (home, legacy) = wonder_home();
        fs::create_dir_all(home.path().join(".cupola/headless")).unwrap();
        assert!(move_legacy(home.path()).is_err());
        assert!(!legacy.is_symlink());
        assert!(home.path().join(".cupola/headless").is_dir());
    }

    #[test]
    fn keychain_items_keep_the_name_they_were_saved_under() {
        let home = Path::new("/Users/ada");
        let saved = "/Users/ada/.wonder";
        assert_eq!(account_in(Some(home), &home.join(".cupola")), saved);
        assert_eq!(account_in(Some(home), &home.join(".wonder")), saved);
        // A development data directory keeps its own items.
        assert_eq!(account_in(Some(home), Path::new("/tmp/dev")), "/tmp/dev");
    }

    #[test]
    fn mac_app_data_moves_into_the_default_directory() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("Library/Application Support/Wonder");
        fs::create_dir_all(old.join("projects/ada")).unwrap();
        fs::write(old.join("projects/ada/work.txt"), "saved").unwrap();
        let new = prepare(home.path()).unwrap();
        assert_eq!(new, home.path().join(".cupola"));
        assert_eq!(
            fs::read_to_string(new.join("projects/ada/work.txt")).unwrap(),
            "saved"
        );
        assert_eq!(
            fs::read_to_string(old.join("projects/ada/work.txt")).unwrap(),
            "saved"
        );
        assert_eq!(prepare(home.path()).unwrap(), new);
    }

    #[test]
    fn mac_app_data_already_linked_to_wonder_is_reached_after_the_move() {
        let (home, legacy) = wonder_home();
        let old = home.path().join("Library/Application Support/Wonder");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&legacy, &old).unwrap();
        move_legacy(home.path()).unwrap();
        assert_eq!(prepare(home.path()).unwrap(), home.path().join(".cupola"));
    }

    #[test]
    fn interrupted_alias_installation_recovers() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("Library/Application Support/Wonder");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(MOVE_MARKER), "").unwrap();
        fs::rename(&old, home.path().join(".cupola")).unwrap();
        prepare(home.path()).unwrap();
        assert!(old.is_symlink());
    }

    #[test]
    fn conflicting_homes_are_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("Library/Application Support/Wonder")).unwrap();
        fs::create_dir_all(home.path().join(".cupola")).unwrap();
        assert!(prepare(home.path()).is_err());
    }

    #[test]
    fn active_supervisor_blocks_migration() {
        let home = tempfile::tempdir().unwrap();
        let service = home
            .path()
            .join("Library/Application Support/Wonder/Service");
        fs::create_dir_all(&service).unwrap();
        let lock = fs::File::create(service.join("launcher.pid")).unwrap();
        lock.lock().unwrap();
        assert!(prepare(home.path()).is_err());
        assert!(!home.path().join(".cupola").exists());
    }
}
