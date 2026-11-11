//! Resumable attachments. Only acknowledged prefixes survive a retry; complete
//! files use the existing conversation-file store. No video processing.
use crate::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

pub(crate) const MAX_VIDEO_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const IDLE_MS: i64 = 24 * 60 * 60 * 1000;
const SWEEP_SECONDS: u64 = 60;
type Failure = (StatusCode, String);
type Result<T> = std::result::Result<T, Failure>;
fn failure(status: StatusCode, text: &str) -> Failure {
    (status, text.into())
}
fn io(_: impl std::fmt::Display) -> Failure {
    failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The upload couldn't be saved. Try again.",
    )
}
fn now() -> i64 {
    Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Begin {
    client_upload_id: String,
    name: String,
    mime_type: String,
    byte_size: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Chunk {
    offset: u64,
    content_base64: String,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    request: Begin,
    received_bytes: u64,
    expires_at: i64,
}

pub(super) fn routes(state: &AppState) -> Router<AppState> {
    start_reaper(state);
    Router::new()
        .route(
            "/api/v1/conversations/{conversation_id}/files/uploads",
            post(begin),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/files/uploads/{upload_id}",
            get(status).patch(append).delete(cancel),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/files/uploads/{upload_id}/complete",
            post(complete),
        )
        .layer(DefaultBodyLimit::max(
            MAX_CHUNK_BYTES.div_ceil(3) * 4 + 4096,
        ))
}

fn validate(request: &Begin) -> Result<()> {
    if uuid::Uuid::parse_str(&request.client_upload_id).is_err()
        || !valid_attachment_name(&request.name)
        || !valid_attachment_mime_type(&request.mime_type)
        || request.byte_size == 0
        || request.sha256.len() != 64
        || !request
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "The upload details are invalid. Attach the file again.",
        ));
    }
    if is_video(&request.mime_type) && !supported_video(&request.mime_type) {
        return Err(failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This video format isn't supported. Attach an MP4, MOV, M4V, 3GP, WebM, Matroska or AVI file.",
        ));
    }
    let limit = if is_video(&request.mime_type) {
        MAX_VIDEO_BYTES
    } else {
        MAX_ATTACHMENT_BYTES as u64
    };
    if request.byte_size > limit {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            if is_video(&request.mime_type) {
                "This video is too large. Videos can be up to 1 GiB (1,073,741,824 bytes)."
            } else {
                "This file is too large. Images and other files can be up to 8 MiB."
            },
        ));
    }
    Ok(())
}
pub(crate) fn is_video(mime: &str) -> bool {
    mime.to_ascii_lowercase().starts_with("video/")
}
fn supported_video(mime: &str) -> bool {
    matches!(
        mime.to_ascii_lowercase().as_str(),
        "video/mp4"
            | "video/quicktime"
            | "video/x-m4v"
            | "video/3gpp"
            | "video/3gpp2"
            | "video/webm"
            | "video/x-matroska"
            | "video/x-msvideo"
    )
}
fn check_generation(state: &AppState, generation: u64) -> Result<()> {
    if media_refs::generation(&state.data_dir) != generation {
        return Err(failure(
            StatusCode::GONE,
            "The workspace was reset. Attach the file again.",
        ));
    }
    Ok(())
}
fn file_id(conversation: &str, upload: &str) -> String {
    deterministic_uuid(&format!("native-upload:{conversation}:{upload}"))
}

// This directory is a sibling of attachments under the same contained media
// root, outside the completed-file orphan sweep. Never follow local redirects.
async fn root(state: &AppState, conversation: &str, upload: &str) -> Result<PathBuf> {
    if uuid::Uuid::parse_str(upload).is_err() {
        return Err(failure(StatusCode::BAD_REQUEST, "Invalid upload identity."));
    }
    if let Some(response) = subagents::reject_user_mutation(state, conversation).await {
        return Err(failure(
            response.status(),
            "Attachments can't be changed in this conversation.",
        ));
    }
    if !projects::is_project(state, conversation).await {
        return Err(failure(StatusCode::NOT_FOUND, "Conversation not found."));
    }
    let workspace = projects::media_workspace(state, conversation)
        .await
        .ok_or_else(|| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "The attachment folder is unavailable.",
            )
        })?;
    let path = attachment_path(&workspace, &file_id(conversation, upload), true)
        .await
        .ok_or_else(|| {
            failure(
                StatusCode::CONFLICT,
                "The attachment folder is unavailable.",
            )
        })?;
    let root = path.parent().unwrap().parent().unwrap().join("uploads");
    match tokio::fs::create_dir(&root).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    let metadata = tokio::fs::symlink_metadata(&root).await.map_err(io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(failure(
            StatusCode::CONFLICT,
            "The upload folder is unavailable.",
        ));
    }
    crate::platform::restrict_dir(&root).map_err(io)?;
    Ok(root)
}
fn paths(root: &FsPath, id: &str) -> (PathBuf, PathBuf) {
    (
        root.join(format!("{id}.json")),
        root.join(format!("{id}.part")),
    )
}
async fn save(root: &FsPath, id: &str, manifest: &Manifest) -> Result<()> {
    let (path, _) = paths(root, id);
    let temp = root.join(format!("{id}.tmp"));
    let bytes = serde_json::to_vec(manifest).map_err(io)?;
    // The identity guard excludes another writer. A previous crashed write is
    // uncommitted and may be replaced; the old manifest remains authoritative.
    let _ = tokio::fs::remove_file(&temp).await;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await
        .map_err(io)?;
    output.write_all(&bytes).await.map_err(io)?;
    output.sync_all().await.map_err(io)?;
    drop(output);
    tokio::fs::rename(&temp, &path).await.map_err(io)
}
async fn erase(root: &FsPath, id: &str) -> Result<()> {
    for suffix in ["part", "json", "tmp", "assembling"] {
        match tokio::fs::remove_file(root.join(format!("{id}.{suffix}"))).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
    }
    Ok(())
}
// A crash can publish complete immutable bytes before saving their database
// record. Cancel/expiry also reclaim that unpublished copy, while keeping every
// file the existing conversation store already owns.
async fn discard(store: &Store, conversation: &str, root: &FsPath, id: &str) -> Result<()> {
    let key = file_id(conversation, id);
    let referenced = store
        .list_conversation_files(conversation)
        .await
        .map_err(io)?
        .iter()
        .any(|file| file.id == key);
    let attachments = root.parent().unwrap().join("attachments");
    if !referenced
        && tokio::fs::symlink_metadata(&attachments)
            .await
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
    {
        match tokio::fs::remove_file(attachments.join(key)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
    }
    erase(root, id).await
}
fn cancelled_path(root: &FsPath, id: &str) -> PathBuf {
    root.join(format!("{id}.cancelled"))
}
async fn cancelled(root: &FsPath, id: &str) -> bool {
    tokio::fs::symlink_metadata(cancelled_path(root, id))
        .await
        .is_ok()
}
async fn mark_cancelled(root: &FsPath, id: &str) -> Result<()> {
    let path = cancelled_path(root, id);
    match crate::platform::create_private_file(&path) {
        Ok(file) => file.sync_all().map_err(io),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(io(e)),
    }
}
async fn load(root: &FsPath, id: &str) -> Result<Manifest> {
    if cancelled(root, id).await {
        return Err(failure(
            StatusCode::NOT_FOUND,
            "This upload was cancelled. Attach the file again.",
        ));
    }
    let (path, _) = paths(root, id);
    let manifest = tokio::task::spawn_blocking(move || attachments::read_regular(&path, 4096))
        .await
        .map_err(io)?
        .map_err(|_| {
            failure(
                StatusCode::NOT_FOUND,
                "This upload is unavailable. Attach the file again.",
            )
        })?;
    let manifest: Manifest = serde_json::from_slice(&manifest).map_err(io)?;
    validate(&manifest.request)?;
    if manifest.request.client_upload_id != id
        || manifest.received_bytes > manifest.request.byte_size
    {
        return Err(failure(
            StatusCode::CONFLICT,
            "The saved upload details changed. Cancel it and attach the file again.",
        ));
    }
    if manifest.expires_at <= now() {
        return Err(failure(
            StatusCode::GONE,
            "This upload expired. Attach the file again.",
        ));
    }
    Ok(manifest)
}
fn progress(
    id: &str,
    manifest: &Manifest,
    file: Option<StoredConversationFile>,
) -> serde_json::Value {
    serde_json::json!({
        "uploadId": id, "receivedBytes": manifest.received_bytes, "totalBytes": manifest.request.byte_size,
        "maxChunkBytes": MAX_CHUNK_BYTES, "expiresAt": manifest.expires_at,
        "state": if file.is_some() { "complete" } else { "uploading" },
        "file": file.map(conversation_file_summary),
    })
}
async fn existing(
    state: &AppState,
    conversation: &str,
    id: &str,
) -> Result<Option<StoredConversationFile>> {
    Ok(state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(io)?
        .into_iter()
        .find(|f| f.id == id))
}
fn matches(file: &StoredConversationFile, request: &Begin) -> bool {
    file.kind == "attachment"
        && file.name == request.name
        && file.mime_type.as_deref() == Some(request.mime_type.as_str())
        && file.byte_size == Some(request.byte_size as i64)
        && file.sha256.as_deref() == Some(request.sha256.as_str())
}
fn finished(id: &str, file: StoredConversationFile) -> serde_json::Value {
    let manifest = Manifest {
        request: Begin {
            client_upload_id: id.into(),
            name: file.name.clone(),
            mime_type: file.mime_type.clone().unwrap_or_default(),
            byte_size: file.byte_size.unwrap_or_default() as u64,
            sha256: file.sha256.clone().unwrap_or_default(),
        },
        received_bytes: file.byte_size.unwrap_or_default() as u64,
        expires_at: now() + IDLE_MS,
    };
    progress(id, &manifest, Some(file))
}

async fn begin(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(request): Json<Begin>,
) -> Result<Json<serde_json::Value>> {
    tokio::spawn(begin_inner(State(state), Path(conversation), Json(request)))
        .await
        .map_err(io)?
}

async fn begin_inner(
    State(state): State<AppState>,
    Path(conversation): Path<String>,
    Json(request): Json<Begin>,
) -> Result<Json<serde_json::Value>> {
    validate(&request)?;
    let id = request.client_upload_id.clone();
    let key = file_id(&conversation, &id);
    let _guard = attachments::identity_guard(&key).await;
    let _capacity = attachments::identity_guard(&format!("upload-capacity:{conversation}")).await;
    let _fence = media_refs::media_write().await;
    let root = root(&state, &conversation, &id).await?;
    if cancelled(&root, &id).await {
        return Err(failure(
            StatusCode::GONE,
            "This upload was cancelled. Attach the file again with a new upload identity.",
        ));
    }
    if let Some(file) = existing(&state, &conversation, &key).await? {
        if !matches(&file, &request) {
            return Err(failure(
                StatusCode::CONFLICT,
                "This upload identity already has different content.",
            ));
        }
        return Ok(Json(finished(&id, file)));
    }
    if tokio::fs::try_exists(paths(&root, &id).0)
        .await
        .map_err(io)?
    {
        let manifest = load(&root, &id).await?;
        if manifest.request != request {
            return Err(failure(
                StatusCode::CONFLICT,
                "This upload identity already has different content.",
            ));
        }
        return Ok(Json(progress(&id, &manifest, None)));
    }
    let mut entries = tokio::fs::read_dir(&root).await.map_err(io)?;
    let mut count = 0;
    while let Some(entry) = entries.next_entry().await.map_err(io)? {
        if entry.path().extension().is_some_and(|x| x == "json") {
            count += 1;
        }
    }
    if count >= 4 {
        return Err(failure(
            StatusCode::TOO_MANY_REQUESTS,
            "Four attachments are already uploading. Finish or cancel one, then retry.",
        ));
    }
    let manifest = Manifest {
        request,
        received_bytes: 0,
        expires_at: now() + IDLE_MS,
    };
    // Persist identity before creating bytes: a crash can never leave an
    // ownerless part, and append can recreate a missing empty part.
    save(&root, &id, &manifest).await?;
    Ok(Json(progress(&id, &manifest, None)))
}
async fn status(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let key = file_id(&conversation, &id);
    let _guard = attachments::identity_guard(&key).await;
    let root = root(&state, &conversation, &id).await?;
    if let Some(file) = existing(&state, &conversation, &key).await? {
        return Ok(Json(finished(&id, file)));
    }
    Ok(Json(progress(&id, &load(&root, &id).await?, None)))
}
async fn append(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
    Json(chunk): Json<Chunk>,
) -> Result<Json<serde_json::Value>> {
    tokio::spawn(append_inner(
        State(state),
        Path((conversation, id)),
        Json(chunk),
    ))
    .await
    .map_err(io)?
}

async fn append_inner(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
    Json(chunk): Json<Chunk>,
) -> Result<Json<serde_json::Value>> {
    let key = file_id(&conversation, &id);
    let _guard = attachments::identity_guard(&key).await;
    let fence = media_refs::media_write().await;
    let generation = media_refs::generation(&state.data_dir);
    let root = root(&state, &conversation, &id).await?;
    let mut manifest = load(&root, &id).await?;
    drop(fence);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&chunk.content_base64)
        .map_err(|_| {
            failure(
                StatusCode::BAD_REQUEST,
                "The upload chunk isn't valid base64.",
            )
        })?;
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Upload chunks must contain between 1 byte and 4 MiB.",
        ));
    }
    let end = chunk
        .offset
        .checked_add(bytes.len() as u64)
        .filter(|end| *end <= manifest.request.byte_size)
        .ok_or_else(|| {
            failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This chunk exceeds the file's declared size.",
            )
        })?;
    if chunk.offset > manifest.received_bytes
        || (chunk.offset < manifest.received_bytes && end > manifest.received_bytes)
    {
        return Err(failure(
            StatusCode::CONFLICT,
            "The upload offset changed. Read its progress and resume at receivedBytes.",
        ));
    }
    let part = paths(&root, &id).1;
    let committed = manifest.received_bytes;
    let offset = chunk.offset;
    tokio::task::spawn_blocking(move || -> Result<()> {
        if !part.exists() && committed == 0 {
            crate::platform::create_private_file(&part).map_err(io)?;
        }
        let mut file = crate::platform::open_regular_nofollow(&part).map_err(io)?;
        let metadata = file.metadata().map_err(io)?;
        if !metadata.is_file() || metadata.len() < committed {
            return Err(failure(
                StatusCode::CONFLICT,
                "The saved upload is incomplete. Cancel it and attach the file again.",
            ));
        }
        if offset < committed {
            file.seek(SeekFrom::Start(offset)).map_err(io)?;
            let mut previous = vec![0; bytes.len()];
            file.read_exact(&mut previous).map_err(io)?;
            if previous != bytes {
                return Err(failure(
                    StatusCode::CONFLICT,
                    "This chunk differs from the bytes already uploaded.",
                ));
            }
            return Ok(());
        }
        drop(file);
        // O_NOFOLLOW also on the writable descriptor; never truncate a redirect.
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let mut output = options.open(&part).map_err(io)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if output.metadata().map_err(io)?.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(failure(
                    StatusCode::CONFLICT,
                    "The upload file is unavailable.",
                ));
            }
        }
        if !output.metadata().map_err(io)?.is_file() {
            return Err(failure(
                StatusCode::CONFLICT,
                "The upload file is unavailable.",
            ));
        }
        output.set_len(committed).map_err(io)?;
        output.seek(SeekFrom::Start(committed)).map_err(io)?;
        output.write_all(&bytes).map_err(io)?;
        output.sync_all().map_err(io)
    })
    .await
    .map_err(io)??;
    #[cfg(test)]
    pause_append(&id).await;
    let _fence = media_refs::media_write().await;
    check_generation(&state, generation)?;
    manifest.received_bytes = committed.max(end);
    manifest.expires_at = now() + IDLE_MS;
    save(&root, &id, &manifest).await?;
    Ok(Json(progress(&id, &manifest, None)))
}
async fn cancel(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
) -> Result<StatusCode> {
    tokio::spawn(cancel_inner(State(state), Path((conversation, id))))
        .await
        .map_err(io)?
}

async fn cancel_inner(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
) -> Result<StatusCode> {
    let key = file_id(&conversation, &id);
    let _guard = attachments::identity_guard(&key).await;
    let root = root(&state, &conversation, &id).await?;
    if existing(&state, &conversation, &key).await?.is_some() {
        return Err(failure(
            StatusCode::CONFLICT,
            "This upload is complete. Remove the attachment from the draft instead.",
        ));
    }
    let _fence = media_refs::media_write().await;
    mark_cancelled(&root, &id).await?;
    discard(&state.store, &conversation, &root, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn complete(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    tokio::spawn(complete_inner(State(state), Path((conversation, id))))
        .await
        .map_err(io)?
}

async fn complete_inner(
    State(state): State<AppState>,
    Path((conversation, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let key = file_id(&conversation, &id);
    let _guard = attachments::identity_guard(&key).await;
    let fence = media_refs::media_write().await;
    let generation = media_refs::generation(&state.data_dir);
    let root = root(&state, &conversation, &id).await?;
    if let Some(file) = existing(&state, &conversation, &key).await? {
        return Ok(Json(finished(&id, file)));
    }
    let manifest = load(&root, &id).await?;
    if manifest.received_bytes != manifest.request.byte_size {
        return Err(failure(
            StatusCode::CONFLICT,
            "This file is still uploading. Resume at receivedBytes before finishing.",
        ));
    }
    let request = manifest.request.clone();
    validate(&request)?;
    let workspace = projects::media_workspace(&state, &conversation)
        .await
        .ok_or_else(|| io("media"))?;
    let destination = attachment_path(&workspace, &key, true)
        .await
        .ok_or_else(|| io("path"))?;
    let part = paths(&root, &id).1;
    drop(fence);
    let staging = part.with_extension("assembling");
    let assembled = staging.clone();
    let old = destination.clone();
    let checked = tokio::task::spawn_blocking(move || -> Result<bool> {
        // Close the writable staging descriptor before immutable publication.
        // Both the assembly and crash-recovery verification run without the
        // global reset fence, under only this upload's identity guard.
        let _ = std::fs::remove_file(&assembled);
        let result = (|| {
            let mut output = crate::platform::create_private_file(&assembled).map_err(io)?;
            verify(
                &part,
                request.byte_size,
                &request.sha256,
                &request.mime_type,
                Some(&mut output),
            )?;
            output.sync_all().map_err(io)?;
            drop(output);
            if old.try_exists().map_err(io)? {
                verify(
                    &old,
                    request.byte_size,
                    &request.sha256,
                    &request.mime_type,
                    None,
                )?;
                Ok(true)
            } else {
                Ok(false)
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(assembled);
        }
        result
    })
    .await
    .map_err(io)?;
    #[cfg(test)]
    pause_append(&id).await;
    let _fence = media_refs::media_write().await;
    check_generation(&state, generation)?;
    let verified_existing = match checked {
        Ok(verified) => verified,
        Err(error) => {
            if matches!(
                error.0,
                StatusCode::UNPROCESSABLE_ENTITY | StatusCode::UNSUPPORTED_MEDIA_TYPE
            ) {
                erase(&root, &id).await?;
            }
            return Err(error);
        }
    };
    let request = &manifest.request;
    // Only publication and its durable ownership record hold the reset fence.
    match tokio::fs::hard_link(&staging, &destination).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && verified_existing => {}
        Err(e) => return Err(io(e)),
    }
    let time = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    state
        .store
        .upsert_conversation_file(
            &key,
            &conversation,
            "attachment",
            &request.name,
            Some(&request.mime_type),
            Some(request.byte_size as i64),
            Some(&request.sha256),
            Some(&attachment_relative_path(&key)),
            "available",
            None,
            None,
            None,
            &time,
        )
        .await
        .map_err(io)?;
    let file = existing(&state, &conversation, &key)
        .await?
        .ok_or_else(|| io("metadata"))?;
    erase(&root, &id).await?;
    Ok(Json(progress(&id, &manifest, Some(file))))
}

/// Stream a single regular descriptor, bounded by the declared length. Optional
/// copying preserves exactly the bytes hashed; never buffer the whole video.
pub(crate) fn verify(
    path: &FsPath,
    size: u64,
    sha: &str,
    mime: &str,
    copy: Option<&mut std::fs::File>,
) -> Result<()> {
    let changed = || {
        failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The file's integrity check failed. Attach it again.",
        )
    };
    if size == 0
        || size
            > if is_video(mime) {
                MAX_VIDEO_BYTES
            } else {
                MAX_ATTACHMENT_BYTES as u64
            }
    {
        return Err(changed());
    }
    let file = crate::platform::open_regular_nofollow(path).map_err(|_| changed())?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() != size {
        return Err(changed());
    }
    let mut reader = CheckedReader {
        input: file.take(size + 1),
        hash: Sha256::new(),
        copy,
        length: 0,
    };
    let matches = if is_video(mime) {
        video_matches(mime, &mut reader, size).map_err(io)?
    } else {
        let mut header = vec![0; size.min(4096) as usize];
        reader.read_exact(&mut header).map_err(io)?;
        content_matches(mime, &header)
    };
    let mut buffer = vec![0; 256 * 1024];
    while reader.read(&mut buffer).map_err(io)? != 0 {}
    if reader.length != size || hex::encode(reader.hash.finalize()) != sha {
        return Err(changed());
    }
    if !matches {
        return Err(failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "This file's contents don't match its declared type. Export it again, then attach it.",
        ));
    }
    Ok(())
}
// Container validation, hashing and copying all consume the same descriptor's
// same byte stream. Read-ahead by the ftyp parser is hashed/copied too, avoiding
// a second-read race with a locally modified staging or attachment file.
struct CheckedReader<'a> {
    input: std::io::Take<std::fs::File>,
    hash: Sha256,
    copy: Option<&'a mut std::fs::File>,
    length: u64,
}
impl Read for CheckedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(buffer)?;
        self.hash.update(&buffer[..count]);
        self.length += count as u64;
        if let Some(output) = self.copy.as_deref_mut() {
            output.write_all(&buffer[..count])?;
        }
        Ok(count)
    }
}

/// MP4/MOV use ftyp brands, WebM/Matroska use EBML, AVI uses RIFF.
/// Unknown video types are refused rather than granting a 1 GiB image bypass.
pub(crate) fn content_matches(mime: &str, bytes: &[u8]) -> bool {
    let mime = mime.to_ascii_lowercase();
    if !is_video(&mime) {
        return agent_images::content_matches(&mime, bytes);
    }
    video_matches(&mime, &mut std::io::Cursor::new(bytes), bytes.len() as u64).unwrap_or(false)
}

fn video_matches(mime: &str, file: &mut impl Read, length: u64) -> std::io::Result<bool> {
    let mime = mime.to_ascii_lowercase();
    if matches!(
        mime.as_str(),
        "video/mp4" | "video/quicktime" | "video/x-m4v" | "video/3gpp" | "video/3gpp2"
    ) {
        return ftyp_matches(&mime, file, length);
    }
    let mut bytes = vec![0; length.min(4096) as usize];
    file.read_exact(&mut bytes)?;
    Ok(match mime.as_str() {
        "video/webm" | "video/x-matroska" => {
            bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
                && bytes
                    .windows(if mime == "video/webm" { 4 } else { 8 })
                    .any(|b| {
                        b == if mime == "video/webm" {
                            &b"webm"[..]
                        } else {
                            &b"matroska"[..]
                        }
                    })
        }
        "video/x-msvideo" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"AVI "),
        _ => false,
    })
}

// Read the complete ftyp box in bounded memory, including compatible brands
// beyond 4 KiB and extended-size boxes. A late image brand must still refuse
// a video declaration. No codec decoding or media processing is performed.
fn ftyp_matches(mime: &str, file: &mut impl Read, length: u64) -> std::io::Result<bool> {
    let mut file = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut header = [0; 8];
    if length < 16 {
        return Ok(false);
    }
    file.read_exact(&mut header)?;
    if &header[4..] != b"ftyp" {
        return Ok(false);
    }
    let size = u32::from_be_bytes(header[..4].try_into().unwrap());
    let (size, head) = if size == 1 {
        file.read_exact(&mut header)?;
        (u64::from_be_bytes(header), 16)
    } else if size == 0 {
        (length, 8)
    } else {
        (u64::from(size), 8)
    };
    if size < head + 8 || size > length || !(size - head).is_multiple_of(4) {
        return Ok(false);
    }
    let mut brand = [0; 4];
    let mut accepted = false;
    let mut image = false;
    for index in 0..(size - head) / 4 {
        file.read_exact(&mut brand)?;
        if index == 1 {
            // minor version, not a brand
            continue;
        }
        image |= matches!(
            &brand,
            b"heic"
                | b"heix"
                | b"heim"
                | b"heis"
                | b"hevc"
                | b"hevx"
                | b"hevm"
                | b"hevs"
                | b"mif1"
                | b"msf1"
                | b"avif"
                | b"avis"
        );
        let gpp = &brand[..3] == b"3gp" && brand[3].is_ascii_digit();
        let gpp2 =
            &brand[..3] == b"3g2" && (brand[3].is_ascii_digit() || matches!(brand[3], b'a'..=b'c'));
        accepted |= match mime {
            "video/quicktime" => &brand == b"qt  ",
            "video/3gpp" => gpp,
            "video/3gpp2" => gpp2,
            _ => {
                gpp || gpp2
                    || matches!(
                        &brand,
                        b"isom"
                            | b"iso2"
                            | b"iso3"
                            | b"iso4"
                            | b"iso5"
                            | b"iso6"
                            | b"iso7"
                            | b"iso8"
                            | b"iso9"
                            | b"mp41"
                            | b"mp42"
                            | b"avc1"
                            | b"M4V "
                            | b"M4VH"
                            | b"M4VP"
                    )
            }
        };
    }
    Ok(accepted && !image)
}

fn start_reaper(state: &AppState) {
    static RUNNING: std::sync::LazyLock<
        std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<projects::ProjectRuntime>>>,
    > = std::sync::LazyLock::new(Default::default);
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    running.retain(|_, owner| owner.strong_count() > 0);
    if running.contains_key(&state.data_dir) {
        return;
    }
    let owner = Arc::downgrade(&state.projects);
    running.insert(state.data_dir.clone(), owner.clone());
    let data = state.data_dir.clone();
    let store = state.store.clone();
    tokio::spawn(async move {
        while owner.strong_count() > 0 {
            reap(&data, &store, now()).await;
            tokio::time::sleep(Duration::from_secs(SWEEP_SECONDS)).await;
        }
    });
}
async fn reap(data: &FsPath, store: &Store, time: i64) {
    let media = data.join("project-media");
    if !tokio::fs::symlink_metadata(&media)
        .await
        .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
    {
        return;
    }
    let Ok(mut folders) = tokio::fs::read_dir(&media).await else {
        return;
    };
    while let Ok(Some(folder)) = folders.next_entry().await {
        let name = folder.file_name().to_string_lossy().into_owned();
        if uuid::Uuid::parse_str(&name).is_err() {
            continue;
        }
        let mut root = media.clone();
        let mut safe = true;
        for segment in [name.as_str(), ".wonder", "uploads"] {
            root.push(segment);
            if !tokio::fs::symlink_metadata(&root)
                .await
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            {
                safe = false;
                break;
            }
        }
        if !safe {
            continue;
        }
        let Ok(mut files) = tokio::fs::read_dir(&root).await else {
            continue;
        };
        while let Ok(Some(entry)) = files.next_entry().await {
            let path = entry.path();
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            else {
                continue;
            };
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("json" | "part" | "tmp" | "assembling" | "cancelled")
            ) {
                continue;
            }
            let _guard = attachments::identity_guard(&file_id(&name, id)).await;
            if cancelled(&root, id).await {
                let _fence = media_refs::media_write().await;
                if discard(store, &name, &root, id).await.is_ok() {
                    let marker = cancelled_path(&root, id);
                    let old = tokio::fs::symlink_metadata(&marker)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .is_some_and(|t| t.as_millis() as i64 + IDLE_MS <= time);
                    if old {
                        let _ = tokio::fs::remove_file(marker).await;
                    }
                }
                continue;
            }
            let (manifest_path, _) = paths(&root, id);
            let manifest = tokio::task::spawn_blocking(move || {
                attachments::read_regular(&manifest_path, 4096)
            })
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok());
            let expired = if let Some(manifest) = manifest {
                manifest.expires_at <= time
            } else {
                tokio::fs::symlink_metadata(&path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .is_some_and(|t| t.as_millis() as i64 + IDLE_MS <= time)
            };
            if expired {
                let _fence = media_refs::media_write().await;
                let _ = discard(store, &name, &root, id).await;
            }
        }
    }
}

#[cfg(test)]
type AppendPause = (String, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
#[cfg(test)]
static APPEND_PAUSE: std::sync::Mutex<Option<AppendPause>> = std::sync::Mutex::new(None);
#[cfg(test)]
async fn pause_append(id: &str) {
    let pause = APPEND_PAUSE
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(identity, _, _)| identity == id)
        .map(|(_, reached, release)| (reached.clone(), release.clone()));
    if let Some((reached, release)) = pause {
        reached.notify_one();
        release.notified().await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{assert_http_contract, call, fixture};
    use serde_json::{json, Value};
    const CHAT: &str = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
    const BASE: &str = "/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/files/uploads";
    fn video(size: usize) -> Vec<u8> {
        let mut bytes = vec![17; size.max(24)];
        bytes[..24].copy_from_slice(b"\0\0\0\x18ftypmp42\0\0\0\0isommp42");
        bytes
    }
    fn request(bytes: &[u8]) -> Value {
        json!({"clientUploadId": uuid::Uuid::new_v4().to_string(), "name": "Screen recording.mp4", "mimeType": "video/mp4",
            "byteSize": bytes.len(), "sha256": hex::encode(Sha256::digest(bytes))})
    }
    async fn chunk(state: &AppState, id: &str, offset: usize, bytes: &[u8]) -> (StatusCode, Value) {
        call(state, "PATCH", &format!("{BASE}/{id}"), json!({"offset":offset, "contentBase64":base64::engine::general_purpose::STANDARD.encode(bytes)})).await
    }
    async fn finish(state: &AppState, id: &str) -> (StatusCode, Value) {
        call(state, "POST", &format!("{BASE}/{id}/complete"), json!({})).await
    }

    // Non-gated boundary/transport contract: no giant Vec, fixture download,
    // live provider or real chat. Send all 1 GiB over TCP to the authenticated
    // production router, restart its listener/store after the first receipt,
    // then stream-verify the stored file and the agent's private file snapshot.
    #[tokio::test]
    async fn one_gib_over_tcp_resumes_and_reaches_the_agent_as_a_file() {
        tokio::time::timeout(Duration::from_secs(600), async {
            async fn serve(state: &AppState) -> (String, tokio::task::JoinHandle<()>) {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let origin = format!("http://{}", listener.local_addr().unwrap());
                let app = crate::router(state.clone());
                let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                (origin, task)
            }
            let (dir, mut state) = fixture().await;
            let bytes = video(MAX_CHUNK_BYTES);
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let mut hash = Sha256::new();
            for _ in 0..MAX_VIDEO_BYTES / MAX_CHUNK_BYTES as u64 {
                hash.update(&bytes);
            }
            let sha = hex::encode(hash.finalize());
            let mut request = request(&bytes);
            request["byteSize"] = json!(MAX_VIDEO_BYTES);
            request["sha256"] = json!(sha);
            let id = request["clientUploadId"].as_str().unwrap().to_owned();
            let (mut origin, mut server) = serve(&state).await;
            let mut client = reqwest::Client::new();
            let response = client
                .post(format!("{origin}{BASE}"))
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "begin: {}",
                response.text().await.unwrap()
            );
            for offset in (0..MAX_VIDEO_BYTES).step_by(MAX_CHUNK_BYTES) {
                let response = client
                    .patch(format!("{origin}{BASE}/{id}"))
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .json(&json!({"offset":offset, "contentBase64":encoded}))
                    .send()
                    .await
                    .unwrap();
                let code = response.status();
                let receipt: Value = response.json().await.unwrap();
                assert_eq!(code, StatusCode::OK, "{receipt}");
                assert_eq!(receipt["receivedBytes"], offset + MAX_CHUNK_BYTES as u64);
                if offset == 0 {
                    server.abort();
                    let _ = server.await;
                    state.store = Store::connect(&format!(
                        "sqlite://{}?mode=rwc",
                        dir.path().join("state.db").display()
                    ))
                    .await
                    .unwrap();
                    (origin, server) = serve(&state).await;
                    client = reqwest::Client::new();
                    let progress: Value = client
                        .get(format!("{origin}{BASE}/{id}"))
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .send()
                        .await
                        .unwrap()
                        .error_for_status()
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    assert_eq!(progress["receivedBytes"], MAX_CHUNK_BYTES);
                }
            }
            let response = client
                .post(format!("{origin}{BASE}/{id}/complete"))
                .header("x-wonder-loopback-capability", &state.loopback_capability)
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            let code = response.status();
            let completed: Value = response.json().await.unwrap();
            assert_eq!(code, StatusCode::OK, "{completed}");
            assert_eq!(completed["receivedBytes"], MAX_VIDEO_BYTES);
            let file = existing(&state, CHAT, completed["file"]["id"].as_str().unwrap())
                .await
                .unwrap()
                .unwrap();
            let workspace = projects::media_workspace(&state, CHAT).await.unwrap();
            let ready = agent_images::for_agent(&workspace, vec![file])
                .await
                .unwrap();
            assert!(ready[0].1.is_none());
            let input = turn_input("Inspect this recording", &workspace, &ready)
                .await
                .unwrap();
            let note = input[1]["text"].as_str().unwrap();
            let path = note.rsplit('`').nth(1).unwrap().to_owned();
            assert!(path.ends_with("Screen recording.mp4"));
            tokio::task::spawn_blocking(move || {
                verify(FsPath::new(&path), MAX_VIDEO_BYTES, &sha, "video/mp4", None)
            })
            .await
            .unwrap()
            .unwrap();
            let staging = root(&state, CHAT, &id).await.unwrap();
            for suffix in ["part", "json", "tmp", "assembling"] {
                assert!(!staging.join(format!("{id}.{suffix}")).exists());
            }
            server.abort();
            let _ = server.await;
            state.projects.shutdown().await;
        })
        .await
        .expect("1 GiB TCP upload contract timed out");
    }

    // Observable contract: an acknowledged prefix survives a host interruption,
    // unacknowledged trailing bytes are discarded, and the same complete file
    // reaches Codex's actual turn/start pipe as a private file path.
    #[tokio::test]
    async fn resume_after_interruption_and_codex_receives_the_video_file() {
        tokio::time::timeout(Duration::from_secs(120), async {
            let (dir, state) = fixture().await;
            state
                .runtime_catalog
                .write()
                .await
                .models
                .iter_mut()
                .find(|model| model.id == "fake")
                .unwrap()
                .default_service_tier = Some("default".into());
            let bytes = video(9 * 1024 * 1024 + 37);
            assert!(bytes.len() > MAX_CHUNK_BYTES);
            let request = request(&bytes);
            assert_http_contract("beginAttachmentUploadRequest", &request);
            let id = request["clientUploadId"].as_str().unwrap();
            let (code, first) = call(&state, "POST", BASE, request.clone()).await;
            assert_eq!(code, StatusCode::OK, "{first}");
            assert_http_contract("attachmentUpload", &first);
            assert_eq!(first["receivedBytes"], 0);
            assert_eq!(
                chunk(&state, id, 0, &bytes[..MAX_CHUNK_BYTES]).await.0,
                StatusCode::OK
            );
            let root = root(&state, CHAT, id).await.unwrap();
            // A process died after writing part of the next request but before
            // committing its receipt. Resume uses the durable manifest offset.
            std::fs::OpenOptions::new()
                .append(true)
                .open(paths(&root, id).1)
                .unwrap()
                .write_all(b"uncommitted tail")
                .unwrap();
            let mut restarted = state.clone();
            restarted.host_epoch = "restarted-host".into();
            restarted.store = Store::connect(&format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("state.db").display()
            ))
            .await
            .unwrap();
            let (code, resumed) = call(&restarted, "GET", &format!("{BASE}/{id}"), json!({})).await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(resumed["receivedBytes"], MAX_CHUNK_BYTES);
            let (_, repeat_begin) = call(&restarted, "POST", BASE, request.clone()).await;
            assert_eq!(repeat_begin["receivedBytes"], MAX_CHUNK_BYTES);
            // Lost chunk receipt: an exact replay is acknowledged once.
            assert_eq!(
                chunk(&restarted, id, 0, &bytes[..MAX_CHUNK_BYTES]).await.0,
                StatusCode::OK
            );
            assert_eq!(
                chunk(&restarted, id, 0, b"different").await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(finish(&restarted, id).await.0, StatusCode::CONFLICT);
            for (index, part) in bytes[MAX_CHUNK_BYTES..].chunks(MAX_CHUNK_BYTES).enumerate() {
                let offset = MAX_CHUNK_BYTES * (index + 1);
                let (code, receipt) = chunk(&restarted, id, offset, part).await;
                assert_eq!(code, StatusCode::OK, "{receipt}");
                assert_eq!(receipt["receivedBytes"], offset + part.len());
            }
            let (code, completed) = finish(&restarted, id).await;
            assert_eq!(code, StatusCode::OK, "{completed}");
            assert_http_contract("attachmentUpload", &completed);
            assert_http_contract("conversationFile", &completed["file"]);
            assert_eq!(completed["state"], "complete");
            assert_eq!(completed["receivedBytes"], bytes.len());
            assert!(!paths(&root, id).0.exists() && !paths(&root, id).1.exists());
            let file_id = completed["file"]["id"].as_str().unwrap();
            assert_eq!(finish(&restarted, id).await.1["file"]["id"], file_id);
            assert_eq!(
                call(&restarted, "POST", BASE, request).await.1["file"]["id"],
                file_id
            );
            let stored = existing(&restarted, CHAT, file_id).await.unwrap().unwrap();
            let workspace = projects::media_workspace(&restarted, CHAT).await.unwrap();
            assert_eq!(
                attachments::stored_bytes(&workspace, &stored)
                    .await
                    .unwrap(),
                bytes
            );
            use std::os::unix::fs::PermissionsExt;
            let stored_path = attachment_path(&workspace, file_id, false).await.unwrap();
            assert_eq!(
                std::fs::metadata(stored_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let wonder_store::MessageInsert::Inserted(message) = restarted
                .store
                .insert_dispatch_message(
                    "owner",
                    "video-contract",
                    "Inspect this recording",
                    "video-hash",
                    CHAT,
                    &[file_id.to_owned()],
                    "now",
                    true,
                )
                .await
                .unwrap()
            else {
                panic!("receipt");
            };
            assert!(restarted
                .store
                .claim_message_for_dispatch(&message.id)
                .await
                .unwrap());
            let mut submitting = false;
            projects::dispatch_inner(&restarted, &message, &mut submitting)
                .await
                .unwrap();
            assert!(submitting);
            let payloads = std::fs::read_to_string(dir.path().join("payloads")).unwrap();
            let turn = payloads
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|v| v["method"] == "turn/start")
                .unwrap();
            let input = turn["params"]["input"].as_array().unwrap();
            assert_eq!(input.len(), 2);
            assert_eq!(input[1]["type"], "text");
            let note = input[1]["text"].as_str().unwrap();
            let path = note.rsplit('`').nth(1).unwrap();
            assert!(path.contains(".wonder/sent/"));
            assert!(path.ends_with("Screen recording.mp4"));
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert!(!note.contains("base64"));
            // The ACP bridge receives the same ordinary localFile contract.
            let ready = agent_images::for_agent(&workspace, vec![stored.clone()])
                .await
                .unwrap();
            assert!(ready[0].1.is_none(), "video isn't buffered inline");
            let acp = attachments::acp_input("Inspect", &workspace, &[ready[0].0.clone()]).unwrap();
            assert_eq!(acp[1]["type"], "localFile");
            assert_eq!(acp[1]["mimeType"], "video/mp4");
            assert_eq!(
                std::fs::read(acp[1]["path"].as_str().unwrap()).unwrap(),
                bytes
            );
            // T3 Code's actual signed HTTP upload consumes a descriptor of the
            // verified snapshot, through the same project send preparation.
            let uploads = projects::t3_code_uploads(&restarted, CHAT, vec![stored])
                .await
                .unwrap();
            assert!(matches!(
                &uploads[0].body,
                crate::t3_code::UploadBody::File { .. }
            ));
            let fake = crate::t3_code::tests::Fake::start().await;
            let (t3, _) = crate::t3_code::tests::connected(&fake).await;
            let sent = t3.upload("video-test", uploads).await.unwrap();
            assert_eq!(sent[0]["type"], "file");
            assert_eq!(fake.uploaded("Screen recording.mp4").unwrap(), bytes);
            // Copies stay with their source record. Once that record is gone,
            // the existing explicit orphan prune reclaims every sent copy.
            let future = std::time::SystemTime::now() + Duration::from_secs(8 * 86400);
            let known = vec![file_id.to_owned()];
            let mut kept = media_refs::orphan_report(
                &state.data_dir,
                &known,
                Duration::from_secs(7 * 86400),
                future,
            );
            media_refs::delete_orphans(&state.data_dir, &mut kept, &known);
            assert!(FsPath::new(path).exists());
            assert!(FsPath::new(acp[1]["path"].as_str().unwrap()).exists());
            let mut gone = media_refs::orphan_report(
                &state.data_dir,
                &[],
                Duration::from_secs(7 * 86400),
                future,
            );
            media_refs::delete_orphans(&state.data_dir, &mut gone, &[]);
            assert!(gone.deleted_bytes >= 3 * bytes.len() as u64);
            assert!(!FsPath::new(path).exists());
            assert!(!FsPath::new(acp[1]["path"].as_str().unwrap()).exists());
            state.projects.shutdown().await;
        })
        .await
        .expect("resumable upload and fake Codex contract timed out");
    }

    #[tokio::test]
    async fn cancel_and_timeout_remove_only_their_partial_files() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (_dir, state) = fixture().await;
            let bytes = video(1024);
            let cancelled = request(&bytes);
            let abandoned = request(&bytes);
            let active = request(&bytes);
            for request in [&cancelled, &abandoned, &active] {
                assert_eq!(
                    call(&state, "POST", BASE, request.clone()).await.0,
                    StatusCode::OK
                );
                assert_eq!(
                    chunk(
                        &state,
                        request["clientUploadId"].as_str().unwrap(),
                        0,
                        &bytes[..100]
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            let id = cancelled["clientUploadId"].as_str().unwrap();
            let root = root(&state, CHAT, id).await.unwrap();
            assert!(paths(&root, id).1.exists());
            assert_eq!(
                call(&state, "DELETE", &format!("{BASE}/{id}"), json!({}))
                    .await
                    .0,
                StatusCode::NO_CONTENT
            );
            assert!(!paths(&root, id).0.exists() && !paths(&root, id).1.exists());
            assert_eq!(
                call(&state, "DELETE", &format!("{BASE}/{id}"), json!({}))
                    .await
                    .0,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                chunk(&state, id, 100, &bytes[100..]).await.0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(finish(&state, id).await.0, StatusCode::NOT_FOUND);
            assert_eq!(
                call(&state, "POST", BASE, cancelled.clone()).await.0,
                StatusCode::GONE,
                "a delayed create retry cannot resurrect a cancelled upload"
            );
            assert!(!paths(&root, id).1.exists());
            let expired_id = abandoned["clientUploadId"].as_str().unwrap();
            let mut manifest = load(&root, expired_id).await.unwrap();
            manifest.expires_at = now() - 1;
            save(&root, expired_id, &manifest).await.unwrap();
            std::fs::write(
                root.join(format!("{expired_id}.assembling")),
                b"interrupted assembly",
            )
            .unwrap();
            let unpublished = root
                .parent()
                .unwrap()
                .join("attachments")
                .join(file_id(CHAT, expired_id));
            std::fs::write(&unpublished, &bytes).unwrap();
            // Exercise the reaper itself (also used at boot), without a request
            // to the abandoned identity and without waiting 24 wall-clock hours.
            reap(&state.data_dir, &state.store, now()).await;
            assert!(!paths(&root, expired_id).0.exists() && !paths(&root, expired_id).1.exists());
            assert!(!root.join(format!("{expired_id}.assembling")).exists());
            assert!(
                !unpublished.exists(),
                "a publish interrupted before metadata is reclaimed too"
            );
            let active_id = active["clientUploadId"].as_str().unwrap();
            assert!(paths(&root, active_id).0.exists() && paths(&root, active_id).1.exists());
            assert!(state
                .store
                .list_conversation_files(CHAT)
                .await
                .unwrap()
                .is_empty());
            state.projects.shutdown().await;
        })
        .await
        .expect("upload cancellation and cleanup timed out");
    }

    #[tokio::test]
    async fn over_limit_is_plain_and_chunks_cannot_extend_the_declared_size() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (_dir, state) = fixture().await;
            let bytes = video(32);
            let mut unsupported = request(&bytes);
            unsupported["mimeType"] = json!("video/unsupported");
            let bad_id = unsupported["clientUploadId"].as_str().unwrap().to_owned();
            let (code, error) = call(&state, "POST", BASE, unsupported).await;
            assert_eq!(code, StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert!(error["error"].as_str().unwrap().contains("MP4"));
            let staging = root(&state, CHAT, &bad_id).await.unwrap();
            assert!(!paths(&staging, &bad_id).0.exists());
            assert!(!paths(&staging, &bad_id).1.exists());
            let mut oversized = request(&bytes);
            oversized["byteSize"] = json!(MAX_VIDEO_BYTES + 1);
            let (code, body) = call(&state, "POST", BASE, oversized.clone()).await;
            assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(
                body["error"],
                "This video is too large. Videos can be up to 1 GiB (1,073,741,824 bytes)."
            );
            oversized["byteSize"] = json!(MAX_VIDEO_BYTES);
            assert_eq!(
                call(&state, "POST", BASE, oversized.clone()).await.0,
                StatusCode::OK,
                "inclusive video limit"
            );
            oversized["clientUploadId"] = json!(uuid::Uuid::new_v4().to_string());
            oversized["mimeType"] = json!("image/png");
            let (code, body) = call(&state, "POST", BASE, oversized).await;
            assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
            assert!(body["error"].as_str().unwrap().contains("8 MiB"));
            let request = request(&bytes);
            let id = request["clientUploadId"].as_str().unwrap();
            call(&state, "POST", BASE, request.clone()).await;
            assert_eq!(
                chunk(&state, id, 1, &bytes[..1]).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                chunk(&state, id, 0, &[0; 33]).await.0,
                StatusCode::PAYLOAD_TOO_LARGE
            );
            assert_eq!(
                chunk(&state, id, usize::MAX, b"x").await.0,
                StatusCode::PAYLOAD_TOO_LARGE
            );
            assert_eq!(chunk(&state, id, 0, &bytes).await.0, StatusCode::OK);
            assert_eq!(finish(&state, id).await.0, StatusCode::OK);
            assert_eq!(
                call(&state, "DELETE", &format!("{BASE}/{id}"), json!({}))
                    .await
                    .0,
                StatusCode::CONFLICT
            );
            state.projects.shutdown().await;
        })
        .await
        .expect("upload size refusal timed out");
    }

    #[tokio::test]
    async fn integrity_and_mime_mismatch_never_publish_a_file() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (_dir, state) = fixture().await;
            let bytes = video(1024);
            for (mime, corrupt, expected) in [
                ("video/mp4", true, StatusCode::UNPROCESSABLE_ENTITY),
                ("video/quicktime", false, StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ] {
                let mut request = request(&bytes);
                request["mimeType"] = json!(mime);
                let id = request["clientUploadId"].as_str().unwrap();
                assert_eq!(
                    call(&state, "POST", BASE, request.clone()).await.0,
                    StatusCode::OK
                );
                let mut changed = bytes.clone();
                if corrupt {
                    changed[100] ^= 1;
                }
                assert_eq!(chunk(&state, id, 0, &changed).await.0, StatusCode::OK);
                let (code, error) = finish(&state, id).await;
                assert_eq!(code, expected, "{error}");
                let root = root(&state, CHAT, id).await.unwrap();
                assert!(!paths(&root, id).0.exists() && !paths(&root, id).1.exists());
                assert!(!root.join(format!("{id}.assembling")).exists());
                let workspace = projects::media_workspace(&state, CHAT).await.unwrap();
                assert!(!attachment_path(&workspace, &file_id(CHAT, id), false)
                    .await
                    .unwrap()
                    .exists());
            }
            assert!(state
                .store
                .list_conversation_files(CHAT)
                .await
                .unwrap()
                .is_empty());
            state.projects.shutdown().await;
        })
        .await
        .expect("upload integrity refusal timed out");
    }

    #[tokio::test]
    async fn a_lost_chunk_request_finishes_its_commit_and_cancel_removes_it() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (_dir, state) = fixture().await;
            let bytes = video(1024);
            let request = request(&bytes);
            let id = request["clientUploadId"].as_str().unwrap().to_owned();
            assert_eq!(call(&state, "POST", BASE, request).await.0, StatusCode::OK);
            let reached = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            *APPEND_PAUSE.lock().unwrap() = Some((id.clone(), reached.clone(), release.clone()));
            let pending = tokio::spawn(append(
                State(state.clone()),
                Path((CHAT.into(), id.clone())),
                Json(Chunk {
                    offset: 0,
                    content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes[..100]),
                }),
            ));
            reached.notified().await;
            // Drop the caller after bytes are synced, but before its receipt.
            pending.abort();
            release.notify_one();
            let (code, progress) = call(&state, "GET", &format!("{BASE}/{id}"), json!({})).await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(
                progress["receivedBytes"], 100,
                "the detached commit survived disconnection"
            );
            *APPEND_PAUSE.lock().unwrap() = None;
            assert_eq!(
                call(&state, "DELETE", &format!("{BASE}/{id}"), json!({}))
                    .await
                    .0,
                StatusCode::NO_CONTENT
            );
            let root = root(&state, CHAT, &id).await.unwrap();
            assert!(!paths(&root, &id).0.exists() && !paths(&root, &id).1.exists());
            assert_eq!(
                chunk(&state, &id, 100, &bytes[100..]).await.0,
                StatusCode::NOT_FOUND
            );
            // Bulk work must not keep a workspace reset (and media writers
            // queued behind it) waiting. A stale chunk/assembly must also
            // never publish metadata or resurrect a reset folder afterward.
            for assembling in [false, true] {
                let next = self::request(&bytes);
                let id = next["clientUploadId"].as_str().unwrap().to_owned();
                assert_eq!(call(&state, "POST", BASE, next).await.0, StatusCode::OK);
                if assembling {
                    assert_eq!(chunk(&state, &id, 0, &bytes).await.0, StatusCode::OK);
                }
                let upload_root = super::root(&state, CHAT, &id).await.unwrap();
                let reached = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Notify::new());
                *APPEND_PAUSE.lock().unwrap() =
                    Some((id.clone(), reached.clone(), release.clone()));
                let pending = {
                    let state = state.clone();
                    let id = id.clone();
                    let bytes = bytes.clone();
                    tokio::spawn(async move {
                        if assembling {
                            finish(&state, &id).await
                        } else {
                            chunk(&state, &id, 0, &bytes).await
                        }
                    })
                };
                reached.notified().await;
                tokio::time::timeout(
                    Duration::from_secs(10),
                    media_refs::reset_media(state.data_dir.clone(), async { Ok::<_, ()>(()) }),
                )
                .await
                .expect("bulk upload held the media reset fence")
                .unwrap();
                release.notify_one();
                assert_eq!(pending.await.unwrap().0, StatusCode::GONE);
                *APPEND_PAUSE.lock().unwrap() = None;
                assert!(
                    !upload_root.exists(),
                    "stale upload recreated its reset folder"
                );
                assert!(existing(&state, CHAT, &file_id(CHAT, &id))
                    .await
                    .unwrap()
                    .is_none());
            }
            state.projects.shutdown().await;
        })
        .await
        .expect("disconnected upload commit and cancellation timed out");
    }

    #[test]
    fn declared_video_container_must_match_its_header() {
        let mp4 = video(24);
        let mov = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
        for (mime, bytes, valid) in [
            ("video/mp4", mp4.as_slice(), true),
            ("VIDEO/MP4", mp4.as_slice(), true),
            ("video/quicktime", mov.as_slice(), true),
            ("video/quicktime", mp4.as_slice(), false),
            ("video/mp4", mov.as_slice(), false),
            ("video/mp4", heic.as_slice(), false),
            ("video/mp4", b"%PDF-1.7 fake video", false),
            ("video/mp4", b"\0\0\0\x18ftypmp42", false),
            ("video/webm", b"\x1a\x45\xdf\xa3\x42\x82webm", true),
            ("video/x-msvideo", b"RIFF\x20\0\0\0AVI ", true),
        ] {
            assert_eq!(content_matches(mime, bytes), valid, "{mime} {bytes:?}");
        }
        // All compatible brands count, including a rejecting image brand at
        // the end of a box larger than the old 4096-byte prefix. Use verify's
        // actual file path as well as the small single-shot validator.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video");
        for (brand, mime, valid) in [
            (*b"iso3", "video/mp4", true),
            (*b"3gp6", "video/mp4", true),
            (*b"3gp6", "video/3gpp", true),
            (*b"3g2a", "video/3gpp2", true),
            (*b"avif", "video/mp4", false),
        ] {
            let mut bytes = vec![0; 8192];
            bytes[..8].copy_from_slice(b"\0\0\x20\0ftyp");
            bytes[8188..].copy_from_slice(&brand);
            assert_eq!(content_matches(mime, &bytes), valid, "{brand:?}");
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                verify(
                    &path,
                    bytes.len() as u64,
                    &hex::encode(Sha256::digest(&bytes)),
                    mime,
                    None
                )
                .is_ok(),
                valid
            );
            bytes[8..12].copy_from_slice(b"isom");
            bytes[8188..].copy_from_slice(b"avif");
            assert!(!content_matches("video/mp4", &bytes));
        }
        let extended = b"\0\0\0\x01ftyp\0\0\0\0\0\0\0\x18iso3\0\0\0\0";
        assert!(content_matches("video/mp4", extended));
        assert!(!content_matches("video/mp4", &extended[..23]));
    }
}
