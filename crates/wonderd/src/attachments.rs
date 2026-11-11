//! Immutable, integrity-checked conversation attachments.
use crate::*;
use serde_json::{json, Value};

/// ACP receives embedded files and local video references. Send the integrity
/// record with the path so its bridge verifies bytes before the provider reads.
pub(super) fn acp_input(
    body: &str,
    workspace: &str,
    files: &[StoredConversationFile],
) -> Result<Vec<Value>, String> {
    let mut input = vec![json!({"type": "text", "text": body})];
    for file in files {
        let relative = file
            .relative_path
            .as_deref()
            .ok_or("An attached file is unavailable.")?;
        let mime = file
            .mime_type
            .as_deref()
            .filter(|mime| valid_attachment_mime_type(mime))
            .ok_or("The attached file's format is missing.")?;
        if mime.starts_with("audio/") {
            return Err("This provider cannot read audio attachments. Dictation can send its transcript as text.".into());
        }
        input.push(json!({
            "type": if mime.starts_with("image/") { "localImage" } else { "localFile" },
            "path": FsPath::new(workspace).join(relative), "name": file.name,
            "mimeType": mime, "sha256": file.sha256, "byteSize": file.byte_size,
        }));
    }
    Ok(input)
}

/// Why a stored file's bytes could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    /// Nothing is at the path any more.
    Missing,
    /// Something other than the recorded bytes is there: different content,
    /// a symlink, a special file or a file over the size limit.
    Changed,
    /// The file could not be opened or read.
    Io,
}

impl ReadFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Missing => "An attached file is missing. Attach it again.",
            Self::Changed => "An attached file changed. Attach it again before retrying.",
            Self::Io => "An attached file couldn't be read. Attach it again.",
        }
    }
}

/// At most `limit` bytes of the regular file at `path`, read through one
/// descriptor. The final component is never followed, a named pipe or device
/// never holds the read waiting, and a file longer than `limit` is refused
/// rather than loaded.
pub(crate) fn read_regular(path: &FsPath, limit: u64) -> Result<Vec<u8>, ReadFailure> {
    use std::io::Read;
    let not_found = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    // A device node must not be opened at all; the descriptor check below
    // covers a swap in between.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(ReadFailure::Changed),
        Ok(_) => {}
        Err(error) if not_found(&error) => return Err(ReadFailure::Missing),
        Err(_) => return Err(ReadFailure::Io),
    }
    // Unix: O_NOFOLLOW and O_NONBLOCK. Windows: the reparse point itself is
    // opened and refused rather than followed.
    let file = match crate::platform::open_regular_nofollow(path) {
        Ok(file) => file,
        Err(error) if not_found(&error) => return Err(ReadFailure::Missing),
        // Whatever took the file's place since the check above (a link or a
        // special file) was opened as itself, never followed; name it.
        Err(_) => {
            return match std::fs::symlink_metadata(path) {
                Ok(metadata) if !metadata.file_type().is_file() => Err(ReadFailure::Changed),
                #[cfg(windows)]
                Ok(metadata)
                    if std::os::windows::fs::MetadataExt::file_attributes(&metadata)
                        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                        != 0 =>
                {
                    Err(ReadFailure::Changed)
                }
                Err(error) if not_found(&error) => Err(ReadFailure::Missing),
                _ => Err(ReadFailure::Io),
            }
        }
    };
    let metadata = file.metadata().map_err(|_| ReadFailure::Io)?;
    if !metadata.is_file() {
        return Err(ReadFailure::Changed);
    }
    if metadata.len() > limit {
        return Err(ReadFailure::Changed);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFailure::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ReadFailure::Changed);
    }
    Ok(bytes)
}

/// The stored file's bytes, only while they are exactly the recorded size
/// and SHA-256. Every send path and the download read attachments here.
pub(crate) fn read_verified(
    path: &FsPath,
    byte_size: i64,
    sha256: &str,
) -> Result<Vec<u8>, ReadFailure> {
    let size = u64::try_from(byte_size)
        .ok()
        .filter(|size| *size <= uploads::MAX_VIDEO_BYTES)
        .ok_or(ReadFailure::Changed)?;
    let bytes = read_regular(path, size)?;
    if bytes.len() as u64 != size || hex::encode(Sha256::digest(&bytes)) != sha256 {
        return Err(ReadFailure::Changed);
    }
    Ok(bytes)
}

pub(super) async fn verified_bytes(
    path: &FsPath,
    file: &StoredConversationFile,
) -> Result<Vec<u8>, String> {
    check_stored_record(file)?;
    let size = file
        .byte_size
        .filter(|size| {
            *size > 0
                && *size
                    <= if uploads::is_video(file.mime_type.as_deref().unwrap_or_default()) {
                        uploads::MAX_VIDEO_BYTES as i64
                    } else {
                        MAX_ATTACHMENT_BYTES as i64
                    }
        })
        .ok_or("The attachment size is invalid.")?;
    let hash = file
        .sha256
        .clone()
        .ok_or("The attachment integrity record is missing.")?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_verified(&path, size, &hash))
        .await
        .map_err(|_| ReadFailure::Io.message().to_owned())?
        .map_err(|failure| failure.message().to_owned())
}

/// A message attachment's verified bytes, read from this conversation's
/// attachment storage.
pub(crate) async fn stored_bytes(
    workspace: &str,
    file: &StoredConversationFile,
) -> Result<Vec<u8>, String> {
    let path = locate_attachment(workspace, &file.id, false)
        .await
        .map_err(|error| match error {
            AttachmentPathError::Missing => ReadFailure::Missing.message(),
            AttachmentPathError::Unsafe => "An attached file is unavailable. Attach it again.",
            AttachmentPathError::Io => ReadFailure::Io.message(),
        })?;
    verified_bytes(&path, file).await
}

/// Publish complete bytes atomically; a crash never leaves a partial file at its
/// durable upload identity. Existing content is verified and never overwritten.
/// Uploads and removals of one file identity run one at a time: a retried
/// upload can recreate the record a removal just deleted, and must not lose
/// its bytes to that removal's cleanup. Entries live while someone holds them.
pub(super) async fn identity_guard(file_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static IDENTITIES: std::sync::LazyLock<
        std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    let lock = {
        let mut identities = IDENTITIES.lock().unwrap_or_else(|e| e.into_inner());
        identities.retain(|_, lock| Arc::strong_count(lock) > 1);
        identities.entry(file_id.to_owned()).or_default().clone()
    };
    lock.lock_owned().await
}

/// A removal of this file identity signals `reached` and waits for `release`.
#[cfg(test)]
pub(super) type RemovalPause = (String, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

/// Test-only pause between a removal's commit and its byte cleanup.
#[cfg(test)]
pub(super) static REMOVAL_PAUSE: std::sync::Mutex<Option<RemovalPause>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) async fn removal_pause(file_id: &str) {
    let pause = REMOVAL_PAUSE
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(id, _, _)| id == file_id)
        .map(|(_, reached, release)| (reached.clone(), release.clone()));
    if let Some((reached, release)) = pause {
        reached.notify_one();
        release.notified().await;
    }
}

/// Holds the media write fence (`media_refs::media_write`); callers must not.
pub(super) async fn write_immutable(destination: &FsPath, bytes: &[u8]) -> Result<bool, String> {
    let _fence = crate::media_refs::media_write().await;
    let temporary = destination.with_file_name(format!(".upload-{}", uuid::Uuid::new_v4()));
    let mut output = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(|e| e.to_string())?;
    let result = async {
        output.write_all(bytes).await.map_err(|e| e.to_string())?;
        output.sync_all().await.map_err(|e| e.to_string())?;
        match tokio::fs::hard_link(&temporary, destination).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let path = destination.to_owned();
                let existing = tokio::task::spawn_blocking(move || {
                    read_regular(&path, MAX_ATTACHMENT_BYTES as u64)
                })
                .await
                .map_err(|e| e.to_string())?;
                if existing.as_deref() != Ok(bytes) {
                    return Err("Attachment identity already contains different bytes.".into());
                }
                Ok(false)
            }
            Err(error) => Err(error.to_string()),
        }
    }
    .await;
    let _ = tokio::fs::remove_file(&temporary).await;
    result
}

/// Pre-dispatch verification uses bounded memory for video. Other attachments
/// retain their existing verified-byte path.
pub(crate) async fn validate_stored(
    workspace: &str,
    file: &StoredConversationFile,
) -> Result<(), String> {
    if !uploads::is_video(file.mime_type.as_deref().unwrap_or_default()) {
        return stored_bytes(workspace, file).await.map(|_| ());
    }
    check_stored_record(file)?;
    let path = locate_attachment(workspace, &file.id, false)
        .await
        .map_err(|_| ReadFailure::Missing.message().to_owned())?;
    let record = file.clone();
    tokio::task::spawn_blocking(move || {
        uploads::verify(
            &path,
            record.byte_size.unwrap_or_default() as u64,
            record.sha256.as_deref().unwrap_or_default(),
            record.mime_type.as_deref().unwrap_or_default(),
            None,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|(_, text)| text)
}

/// Stored attachment identity/state checks shared by buffered and streamed reads.
pub(crate) fn check_stored_record(file: &StoredConversationFile) -> Result<(), String> {
    if file.kind != "attachment"
        || file.state != "available"
        || file.relative_path.as_deref() != Some(attachment_relative_path(&file.id).as_str())
    {
        return Err("An attached file is unavailable. Attach it again.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A12-10/A12-31: the one stored-attachment read refuses anything but the
    // recorded bytes in a regular file, without following or waiting.
    #[test]
    fn stored_reads_refuse_replacements_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attachment");
        let bytes = b"attached bytes";
        let sha = hex::encode(Sha256::digest(bytes));
        let size = bytes.len() as i64;
        assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Missing));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_verified(&path, size, &sha).unwrap(), bytes);

        std::fs::write(&path, b"Attached bytes").unwrap();
        assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Changed));
        std::fs::write(&path, vec![b'x'; MAX_ATTACHMENT_BYTES + 1]).unwrap();
        assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Changed));
        assert_eq!(
            read_regular(&path, MAX_ATTACHMENT_BYTES as u64),
            Err(ReadFailure::Changed)
        );
        #[cfg(unix)]
        {
            let original = dir.path().join("original");
            std::fs::write(&original, bytes).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&original, &path).unwrap();
            assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Changed));
            std::fs::remove_file(&path).unwrap();
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Changed));
        }
        // Creating a symlink needs Developer Mode or elevation; without it
        // there is no link to refuse.
        #[cfg(windows)]
        {
            let original = dir.path().join("original");
            std::fs::write(&original, bytes).unwrap();
            std::fs::remove_file(&path).unwrap();
            if std::os::windows::fs::symlink_file(&original, &path).is_ok() {
                assert_eq!(read_verified(&path, size, &sha), Err(ReadFailure::Changed));
                assert!(crate::platform::open_regular_nofollow(&path).is_err());
            }
        }
    }
}
