//! One spelling for a folder's canonical path.
//!
//! `std::fs::canonicalize` returns `\\?\C:\Work` on Windows, while
//! `current_exe()`, saved Project roots and paths a person types use
//! `C:\Work`. `Path` comparisons treat those as different folders, so every
//! comparison of canonical paths goes through [`canonicalize`] (or
//! [`simplified`] for a path that is already canonical). Other platforms, and
//! Windows paths that cannot be written as a plain drive path (network
//! shares, device paths, names over the classic length limit), are unchanged.

use std::{
    io,
    path::{Component, Path, PathBuf, Prefix},
};

/// `std::fs::canonicalize`, with a Windows `\\?\C:\…` result written as
/// `C:\…`.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(simplified)
}

/// [`canonicalize`] without blocking the async runtime.
pub async fn canonicalize_async(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    tokio::fs::canonicalize(path).await.map(simplified)
}

/// Writes a Windows verbatim drive path (`\\?\C:\…`) as `C:\…` when that names
/// the same folder. Anything else is returned as given.
pub fn simplified(path: impl Into<PathBuf>) -> PathBuf {
    let path = path.into();
    let plain = dunce::simplified(&path);
    // dunce also rewrites `\\?\UNC\server\share`; shares stay verbatim so a
    // network folder keeps the one spelling it has always had.
    if matches!(
        plain.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    ) {
        plain.to_path_buf()
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_unix_paths_are_unchanged() {
        for path in ["/", "/tmp/work", "relative/dir", ""] {
            assert_eq!(simplified(path), PathBuf::from(path));
        }
    }

    #[test]
    fn canonicalize_of_a_real_folder_matches_std_up_to_the_prefix() {
        let folder = tempfile::tempdir().unwrap();
        let canonical = canonicalize(folder.path()).unwrap();
        assert_eq!(
            canonical,
            simplified(std::fs::canonicalize(folder.path()).unwrap())
        );
        assert_eq!(canonicalize(&canonical).unwrap(), canonical);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_and_plain_drive_paths_become_one_spelling() {
        let verbatim = PathBuf::from(r"\\?\C:\Users\me\Cupola\versions\1.0");
        let plain = PathBuf::from(r"C:\Users\me\Cupola\versions\1.0");
        assert_eq!(simplified(verbatim.clone()), plain);
        assert_eq!(simplified(plain.clone()), plain);
        assert!(plain.starts_with(simplified(verbatim)));
    }

    #[cfg(windows)]
    #[test]
    fn shares_and_devices_stay_verbatim() {
        for path in [
            r"\\?\UNC\server\share\work",
            r"\\.\pipe\x",
            r"\\server\share",
        ] {
            assert_eq!(simplified(path), PathBuf::from(path));
        }
        // Over the classic limit a plain spelling would not open.
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(simplified(long.clone()), PathBuf::from(long));
    }
}
