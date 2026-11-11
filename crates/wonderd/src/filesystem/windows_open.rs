//! EXPERIMENTAL Windows counterpart of the `openat`/`O_NOFOLLOW` workspace
//! opens in `filesystem.rs`.
//!
//! Each path component is opened from the drive root with
//! `FILE_FLAG_OPEN_REPARSE_POINT` (never following a symlink or junction) and
//! without `FILE_SHARE_DELETE`. Every ancestor handle stays open until the
//! final open completes, so no component of the chain can be renamed, deleted
//! or swapped for a link while it is walked: renaming needs DELETE access,
//! which an open handle without FILE_SHARE_DELETE refuses. A component that is
//! a reparse point is refused like a symlink on Unix.
use super::{disk_error, too_many_entries_error, BrowseError, WorkspaceRoot};
use axum::http::StatusCode;
use std::{
    ffi::OsStr,
    fs, io,
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, GetFileInformationByHandleEx,
    FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

const SYMLINK: BrowseError = (
    StatusCode::FORBIDDEN,
    "Symlinks cannot be opened in Cupola.",
);

/// An open chain: the ancestors are held only to pin the path.
struct Chain {
    _ancestors: Vec<fs::File>,
    leaf: fs::File,
    path: PathBuf,
}

fn open_one(path: &Path, directory: bool) -> Result<fs::File, BrowseError> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    let file = options.open(path).map_err(open_error)?;
    let attributes = file.metadata().map_err(disk_error)?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SYMLINK);
    }
    if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        return Err((
            StatusCode::FORBIDDEN,
            "Special files cannot be opened in Cupola.",
        ));
    }
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if directory && !is_directory {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    if !directory && is_directory {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file."));
    }
    Ok(file)
}

fn open_error(error: io::Error) -> BrowseError {
    // ERROR_DIRECTORY (267): a file where a folder was expected.
    match error.raw_os_error() {
        Some(267) => (StatusCode::BAD_REQUEST, "Choose a folder or regular file."),
        _ => disk_error(error),
    }
}

/// Opens `root` and then each of `relative`'s components as folders.
fn open_directory_chain(root: &Path, relative: &Path) -> Result<Chain, BrowseError> {
    if !root.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            "This workspace path must be absolute.",
        ));
    }
    let mut ancestors = Vec::new();
    let mut path = PathBuf::new();
    let mut leaf: Option<fs::File> = None;
    let normal = |component: Component<'_>| -> Result<Option<PathBuf>, BrowseError> {
        Ok(match component {
            Component::Prefix(_) | Component::RootDir => None,
            Component::Normal(name) => Some(PathBuf::from(name)),
            _ => return Err((StatusCode::BAD_REQUEST, "This workspace path is invalid.")),
        })
    };
    for component in root.components() {
        match normal(component)? {
            None => {
                path.push(component.as_os_str());
                if matches!(component, Component::RootDir) {
                    // The volume root itself (C:\) cannot be a reparse point.
                    leaf = Some(open_one(&path, true)?);
                }
            }
            Some(name) => {
                path.push(name);
                if let Some(previous) = leaf.replace(open_one(&path, true)?) {
                    ancestors.push(previous);
                }
            }
        }
    }
    for component in relative.components() {
        let Some(name) = normal(component)? else {
            return Err((StatusCode::BAD_REQUEST, "This workspace path is invalid."));
        };
        path.push(name);
        if let Some(previous) = leaf.replace(open_one(&path, true)?) {
            ancestors.push(previous);
        }
    }
    let leaf = leaf.ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
    Ok(Chain {
        _ancestors: ancestors,
        leaf,
        path,
    })
}

fn open_file_in(chain: Chain, name: &OsStr) -> Result<fs::File, BrowseError> {
    let file = open_one(&chain.path.join(name), false)?;
    drop(chain);
    Ok(file)
}

pub(super) fn open_workspace_file(
    root: &WorkspaceRoot,
    relative: &Path,
) -> Result<fs::File, BrowseError> {
    if !root.is_directory && !relative.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Choose a regular file."));
    }
    if relative.as_os_str().is_empty() {
        let parent = root
            .path
            .parent()
            .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
        let name = root
            .path
            .file_name()
            .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
        return open_file_in(open_directory_chain(parent, Path::new(""))?, name);
    }
    let chain = open_directory_chain(&root.path, relative.parent().unwrap_or(Path::new("")))?;
    let name = relative
        .file_name()
        .ok_or((StatusCode::BAD_REQUEST, "This workspace path is invalid."))?;
    open_file_in(chain, name)
}

pub(super) fn open_workspace_directory(
    root: &WorkspaceRoot,
    relative: &Path,
) -> Result<fs::File, BrowseError> {
    if !root.is_directory {
        return Err((StatusCode::BAD_REQUEST, "Choose a folder to browse."));
    }
    let chain = open_directory_chain(&root.path, relative)?;
    Ok(chain.leaf)
}

/// Lists a folder from its handle (no path lookup), skipping links, devices
/// and names that are not valid Unicode.
pub(super) fn read_directory_entries(
    directory: &fs::File,
    max_entries: usize,
) -> Result<Vec<(String, bool, Option<u64>)>, BrowseError> {
    let handle = directory.as_raw_handle();
    let mut entries = Vec::new();
    // u64 alignment for FILE_FULL_DIR_INFO.
    let mut buffer = vec![0u64; 8 * 1024];
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: the buffer outlives the call and its size is passed in bytes.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle as _,
                class,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        };
        class = FileFullDirectoryInfo;
        if ok == 0 {
            let error = io::Error::last_os_error();
            // ERROR_NO_MORE_FILES
            if error.raw_os_error() == Some(18) {
                return Ok(entries);
            }
            return Err(disk_error(error));
        }
        let mut offset = 0usize;
        loop {
            // SAFETY: the kernel wrote a chain of FILE_FULL_DIR_INFO records
            // starting at offset 0, each linked by NextEntryOffset.
            let record = unsafe {
                &*(buffer.as_ptr().cast::<u8>().add(offset) as *const FILE_FULL_DIR_INFO)
            };
            let name_units = record.FileNameLength as usize / 2;
            let name_ptr = record.FileName.as_ptr();
            // SAFETY: FileNameLength bytes of UTF-16 follow the fixed fields.
            let name = unsafe { std::slice::from_raw_parts(name_ptr, name_units) };
            let name = std::ffi::OsString::from_wide(name);
            let attributes = record.FileAttributes;
            let size = record.EndOfFile.max(0) as u64;
            let next = record.NextEntryOffset as usize;
            if let Some(name) = name.to_str().filter(|n| *n != "." && *n != "..") {
                let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                let skip = attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0;
                if !skip {
                    if entries.len() >= max_entries {
                        return Err(too_many_entries_error());
                    }
                    entries.push((
                        name.to_owned(),
                        is_directory,
                        (!is_directory).then_some(size),
                    ));
                }
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}
