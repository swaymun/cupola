//! Whether a paired iPhone may take control of this Mac. The macOS computer
//! helper reads `control-preferences.json` in the directory the launcher
//! names in `WONDER_SERVICE_DIR`, and fails closed on anything it does not
//! trust (`native/computer-use/.../ControlPreferences.swift`). This mirrors
//! its checks, so `cupola doctor` and `cupola computer control` report what
//! the helper will decide.
use super::Result;
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub(super) const FILE_NAME: &str = "control-preferences.json";
const MAXIMUM_FILE_BYTES: u64 = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlPreference {
    Enabled,
    Disabled,
    /// The file or its directory is not one the helper trusts, so control is
    /// refused until the preference is written again.
    Unavailable,
}

/// The directory is owned by this user, private and reached without symlinks.
fn trusted_directory(dir: &Path) -> bool {
    let mut current = std::path::PathBuf::new();
    for component in dir.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {}
            _ => return false,
        }
    }
    fs::symlink_metadata(dir).is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o700 == 0o700
    })
}

pub(super) fn read(dir: &Path) -> ControlPreference {
    // Never chosen: the helper refuses control, and nothing needs repair.
    if dir.is_absolute() && !dir.exists() && fs::symlink_metadata(dir).is_err() {
        return ControlPreference::Disabled;
    }
    if !dir.is_absolute() || !trusted_directory(dir) {
        return ControlPreference::Unavailable;
    }
    let path = dir.join(FILE_NAME);
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ControlPreference::Disabled
        }
        Err(_) => return ControlPreference::Unavailable,
    };
    let trusted = file.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.mode() & 0o400 != 0
            && metadata.len() <= MAXIMUM_FILE_BYTES
    });
    let mut text = String::new();
    if !trusted
        || file
            .by_ref()
            .take(MAXIMUM_FILE_BYTES)
            .read_to_string(&mut text)
            .is_err()
    {
        return ControlPreference::Unavailable;
    }
    let Ok(serde_json::Value::Object(document)) = serde_json::from_str(&text) else {
        return ControlPreference::Unavailable;
    };
    let mut keys = document.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    match (
        keys == ["allowControlFromPairedDevices", "version"],
        document["version"].as_u64(),
        document["allowControlFromPairedDevices"].as_bool(),
    ) {
        (true, Some(1), Some(true)) => ControlPreference::Enabled,
        (true, Some(1), Some(false)) => ControlPreference::Disabled,
        _ => ControlPreference::Unavailable,
    }
}

/// Writes the preference the way the helper's own store does: a private
/// directory and a private file, replaced in one rename.
pub(super) fn write(dir: &Path, allow: bool) -> Result<()> {
    fs::create_dir_all(dir)?;
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link; the computer helper only trusts a real directory there.",
            dir.display()
        )
        .into());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let document = format!(r#"{{"allowControlFromPairedDevices":{allow},"version":1}}"#);
    super::write_private(&dir.join(FILE_NAME), document.as_bytes())?;
    if read(dir)
        != if allow {
            ControlPreference::Enabled
        } else {
            ControlPreference::Disabled
        }
    {
        return Err(format!(
            "The computer helper would not trust {}: make every folder on that path a real, private folder owned by you.",
            dir.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        // macOS temp dirs sit behind the /var -> /private/var symlink, which
        // the helper rejects; name the real path, as the launcher does.
        let dir = wonderd::canonical_path::canonicalize(root.path())
            .unwrap()
            .join("Service");
        (root, dir)
    }

    #[test]
    fn written_preference_reads_back_and_matches_the_helper_document() {
        let (_root, dir) = service_dir();
        assert_eq!(read(&dir), ControlPreference::Disabled, "no directory");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(read(&dir), ControlPreference::Disabled, "no file");
        write(&dir, true).unwrap();
        assert_eq!(read(&dir), ControlPreference::Enabled);
        // The exact bytes the Swift store's sorted-key encoder writes.
        assert_eq!(
            fs::read_to_string(dir.join(FILE_NAME)).unwrap(),
            r#"{"allowControlFromPairedDevices":true,"version":1}"#
        );
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME)).unwrap().mode() & 0o777,
            0o600
        );
        write(&dir, false).unwrap();
        assert_eq!(read(&dir), ControlPreference::Disabled);
    }

    #[test]
    fn untrusted_preferences_are_unavailable_like_the_helper() {
        let (_root, dir) = service_dir();
        write(&dir, true).unwrap();
        let file = dir.join(FILE_NAME);
        for (contents, mode) in [
            (
                r#"{"allowControlFromPairedDevices":true,"version":2}"#,
                0o600,
            ),
            (
                r#"{"allowControlFromPairedDevices":"yes","version":1}"#,
                0o600,
            ),
            (
                r#"{"allowControlFromPairedDevices":true,"version":1,"x":1}"#,
                0o600,
            ),
            ("not json", 0o600),
            (
                r#"{"allowControlFromPairedDevices":true,"version":1}"#,
                0o644,
            ),
        ] {
            fs::write(&file, contents).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read(&dir),
                ControlPreference::Unavailable,
                "{contents} {mode:o}"
            );
        }
        write(&dir, true).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read(&dir),
            ControlPreference::Unavailable,
            "shared directory"
        );
        // Writing again repairs the directory.
        write(&dir, true).unwrap();
        assert_eq!(read(&dir), ControlPreference::Enabled);

        let linked = dir.with_file_name("Linked");
        std::os::unix::fs::symlink(&dir, &linked).unwrap();
        assert_eq!(
            read(&linked),
            ControlPreference::Unavailable,
            "symlinked directory"
        );
        assert!(write(&linked, true).is_err());
    }
}
