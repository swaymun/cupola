//! Convert inline tool media to verified, authenticated conversation files before
//! projection truncation. External URLs remain inert; they are never fetched.
use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use wonder_store::MediaSourceLocation;

pub(super) async fn normalize(
    state: &AppState,
    conversation: &str,
    turn: &str,
    item: &Value,
) -> Result<Value, String> {
    let mut item = item.clone();
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if item_id.is_empty() {
        return Ok(item);
    }
    if item.get("type").and_then(Value::as_str) == Some("userMessage") {
        let files = t3_attachments(state.t3_code.attachments_dir().as_deref(), &mut item);
        let mut item = user_images(state, conversation, turn, &item_id, item).await?;
        if !files.is_empty() {
            user_files(state, conversation, turn, &item_id, &mut item, files).await?;
        }
        return Ok(item);
    }
    // T3 Code's pages (owner, Oct 22): a published page becomes a verified
    // text/html file the phone draws natively above the reply, and a preview's
    // screenshot stays in the call's details rather than joining the reply.
    // Neither call's Input carries the page source.
    if let Some(tool) = t3_page_tool(&item) {
        if tool == "html_render" {
            let page = html_render(state, conversation, turn, &item_id, &item).await?;
            if !page.is_null() {
                item["htmlRender"] = page;
            }
        } else {
            item["mediaInDetails"] = json!(true);
        }
        if let Some(html) = item.pointer_mut("/arguments/html") {
            let size = html.as_str().map_or(0, str::len);
            *html = json!(format!("[HTML page, {} KB]", size.div_ceil(1024).max(1)));
        }
    }
    // Cupola's own `wonder_show_visual` kept its visual when it ran; the call
    // names it in its result, and the phone shows it like a T3 page.
    if is_show_visual(&item) {
        if let Some(page) = shown_visual(state, conversation, &item).await? {
            item["htmlRender"] = page;
        }
        if let Some(html) = item.pointer_mut("/arguments/html") {
            let size = html.as_str().map_or(0, str::len);
            *html = json!(format!("[HTML page, {} KB]", size.div_ceil(1024).max(1)));
        }
        if let Some(document) = item.pointer_mut("/arguments/components") {
            let count = document
                .get("components")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            *document = json!(format!(
                "[{count} component{}]",
                if count == 1 { "" } else { "s" }
            ));
        }
        // A preview posts nothing; its pictures stay in the call's details,
        // in whichever shape the runtime carried them (Codex and Claude
        // `inputImage`, the ACP bridge's MCP `image`).
        let mut pictures = Vec::new();
        for key in ["result", "contentItems", "output"] {
            if let Some(value) = item.get(key) {
                collect(value, vec![key.to_owned()], &mut pictures, 0);
            }
        }
        if item.get("htmlRender").is_some() || pictures.is_empty() {
            return Ok(item);
        }
        item["mediaInDetails"] = json!(true);
    }
    // Generated images carry bare base64 or a Codex attachment-store identity.
    // Normalize before projection truncates strings, including history refresh.
    // Paired clients use the retained bytes; never resolve the runtime savedPath.
    // The file the tool wrote names the preview; only its name leaves the Mac.
    let mut name_hint = None;
    if item.get("type").and_then(Value::as_str) == Some("imageGeneration") {
        name_hint = item
            .as_object_mut()
            .unwrap()
            .remove("savedPath")
            .and_then(|path| path.as_str().and_then(file_name));
        let file_only = item.get("fileId").is_some_and(Value::is_string)
            && item
                .get("result")
                .is_none_or(|result| result.is_null() || result.as_str() == Some(""));
        if file_only {
            // Codex 0.164.0 exposes thread attachment metadata, but no
            // app-server method to resolve these image bytes. An empty result
            // beside fileId is also a file reference during Codex's migration.
            if item.get("status").and_then(Value::as_str) == Some("completed") {
                item["result"] = json!({"content":[unavailable(
                    "This Codex version keeps generated images on your computer; open it in Codex"
                )]});
            }
        } else if item.get("result").is_some_and(Value::is_string) {
            let encoded = item["result"].take();
            item["result"] = json!({"content":[{"type":"image","data":encoded}]});
        } else if item.get("result").is_none_or(Value::is_null)
            && item.get("status").and_then(Value::as_str) == Some("completed")
        {
            item["result"] =
                json!({"content":[unavailable("The image was not returned by the generator")]});
        }
    }
    // The image file a Codex imageView showed: its preview is kept as a
    // reference to that file when it holds the very bytes shown.
    let file_hint = (item.get("type").and_then(Value::as_str) == Some("imageView"))
        .then(|| item.get("path").and_then(Value::as_str).map(str::to_owned))
        .flatten();
    // An imageView naming another chat's stored media (or Cupola's preview
    // cache) is not shown here, even with its bytes inline.
    if let Some(path) = &file_hint {
        let own = crate::projects::media_workspace(state, conversation).await;
        if private_elsewhere(&state.data_dir, own.as_deref(), path) {
            item["result"] = json!({"content":[unavailable(PRIVATE_MEDIA)]});
            return Ok(item);
        }
    }
    // Bytes the imageView file reader already verified, used as the first
    // preview without a base64 round trip: (bytes, mime, name, sha256).
    let mut loaded_image = None;
    // imageView carries a host-local path, not inline image bytes. Resolve it
    // before event redaction and keep a verified reference for paired clients.
    if item.get("type").and_then(Value::as_str) == Some("imageView") && item.get("result").is_none()
    {
        let source_id = format!("tool-media:{turn}:{item_id}:0");
        if let Some(file) = state
            .store
            .list_conversation_files(conversation)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|file| file.source_id.as_deref() == Some(&source_id) && file.state == "available")
        {
            let size = known_size(state, conversation, &file).await;
            item["result"] = json!({"content":[artifact(&file, size)]});
            if file.relative_path.is_none() {
                retain_recorded(state, conversation, &file).await;
            }
            return Ok(item);
        }
        let source = item
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        name_hint = file_name(&source);
        // Without retained storage the thread still opens; only this
        // preview is missing.
        if crate::projects::media_workspace(state, conversation)
            .await
            .is_none()
        {
            item["result"] = json!({"content":[unavailable(transient(NO_PREVIEW_STORAGE))]});
            return Ok(item);
        }
        let loaded = tokio::task::spawn_blocking(move || {
            local_image(&source).map(|(bytes, mime, name)| {
                let digest = hex::encode(Sha256::digest(&bytes));
                (bytes, mime, name, digest)
            })
        })
        .await
        .map_err(|e| e.to_string())?;
        item["result"] = match loaded {
            Ok(file) => {
                let block = json!({"content":[{"type":"image","mimeType":file.1}]});
                loaded_image = Some(file);
                block
            }
            Err(reason) => json!({"content":[unavailable(reason)]}),
        };
    }
    let mut paths = Vec::new();
    for key in ["result", "contentItems", "output"] {
        if let Some(value) = item.get(key) {
            collect(value, vec![key.to_owned()], &mut paths, 0);
        }
    }
    if paths.is_empty() {
        return Ok(item);
    }
    let Some(storage) = crate::projects::media_workspace(state, conversation).await else {
        for path in &paths {
            if let Some(value) = at_mut(&mut item, path) {
                *value = unavailable(transient(NO_PREVIEW_STORAGE));
            }
        }
        return Ok(item);
    };
    let existing: HashMap<String, wonder_store::StoredConversationFile> = state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|file| (file.id.clone(), file))
        .collect();
    let mut transcript = None;
    let mut total = 0;
    for (index, path) in paths.iter().enumerate() {
        let Some(value) = at_mut(&mut item, path) else {
            continue;
        };
        // Decoding and hashing happen once, on a blocking thread; the digest
        // names the file, checks its copy and is recorded.
        let preloaded = loaded_image.take().filter(|_| index == 0);
        let verified = preloaded.is_some();
        let decoded = if let Some(file) = preloaded {
            Ok(file)
        } else if index < 16 {
            let raw = std::mem::take(value);
            tokio::task::spawn_blocking(move || {
                decode(&raw).map(|(bytes, mime, name)| {
                    let digest = hex::encode(Sha256::digest(&bytes));
                    (bytes, mime, name, digest)
                })
            })
            .await
            .map_err(|e| e.to_string())?
        } else {
            Err("Too many previews in this result")
        };
        let (bytes, mime, name, digest) = match decoded {
            Ok(file) if total + file.0.len() <= 16 * 1024 * 1024 => file,
            Ok(_) => {
                *value = unavailable("This result exceeds the preview size limit");
                continue;
            }
            Err(reason) => {
                *value = unavailable(reason);
                continue;
            }
        };
        total += bytes.len();
        let source_id = format!("tool-media:{turn}:{item_id}:{index}");
        let id_seed = format!("tool-media:{conversation}:{turn}:{item_id}:{index}");
        let id = media_id(&id_seed, &mime, &digest);
        // A picture already kept by reference was verified when it was
        // recorded; a refresh only names it again, keeping its retained
        // preview if it was recorded before history kept one.
        if let Some(file) = existing
            .get(&id)
            .filter(|file| file.state == "available" && file.relative_path.is_none())
        {
            if let Ok(Some(source)) = state.store.media_source(&file.id).await {
                *value = artifact(file, source.width.zip(source.height));
                let (media, id) = (std::path::PathBuf::from(&storage), id.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    media_refs::retain(&media, &id, &bytes);
                })
                .await;
                continue;
            }
        }
        let checked = if verified {
            Ok((bytes, mime, name))
        } else {
            tokio::task::spawn_blocking(move || {
                validate(&bytes, &mime).map(|_| (bytes, mime, name))
            })
            .await
            .map_err(|e| e.to_string())?
        };
        let (mut bytes, mime, name) = match checked {
            Ok(file) => file,
            Err(reason) => {
                *value = unavailable(reason);
                continue;
            }
        };
        let name = match &name_hint {
            Some(hint) if paths.len() == 1 => named_like(hint, &name),
            _ => name,
        };
        // An agent's picture that already lives on this Mac is kept by
        // reference, with a small retained preview; anything else, or one
        // already copied, is copied as before.
        let copied = existing
            .get(&id)
            .is_some_and(|file| file.relative_path.is_some());
        if media_refs::resizable(&mime) && !copied {
            if transcript.is_none() {
                transcript = Some(transcript_of(state, conversation).await);
            }
            let hint = file_hint.clone().filter(|_| paths.len() == 1);
            let transcript = transcript.clone().flatten();
            let (media, kept_id) = (std::path::PathBuf::from(&storage), id.clone());
            let (found, returned) = tokio::task::spawn_blocking(move || {
                let found = hint
                    .and_then(|hint| media_refs::file_holding(&hint, &bytes))
                    .map(|path| MediaSourceLocation::File { path })
                    .or_else(|| {
                        let transcript = transcript?;
                        let (offset, length) =
                            media_refs::locate_in_transcript(&transcript, &bytes)?;
                        Some(MediaSourceLocation::Transcript {
                            path: transcript.to_str()?.to_owned(),
                            offset,
                            length,
                        })
                    });
                // The retained preview is on disk before the record names it.
                if found.is_some() {
                    media_refs::retain(&media, &kept_id, &bytes);
                }
                (found, bytes)
            })
            .await
            .map_err(|e| e.to_string())?;
            bytes = returned;
            if let Some(location) = found {
                if let Ok(reference) = reference(
                    state,
                    conversation,
                    &id,
                    &source_id,
                    &name,
                    &mime,
                    &bytes,
                    &digest,
                    location,
                )
                .await
                {
                    *value = reference;
                    continue;
                }
            }
        }
        // A preview that cannot be saved costs only that preview, never the
        // item or the rest of the thread's history.
        *value = store_hashed(
            state,
            conversation,
            &storage,
            &id_seed,
            &source_id,
            &name,
            &mime,
            &bytes,
            &digest,
        )
        .await
        .unwrap_or_else(|_| unavailable(transient(PREVIEW_NOT_SAVED)));
    }
    Ok(item)
}

const NO_PREVIEW_STORAGE: &str = "Cupola can't keep previews for this thread";
const PREVIEW_NOT_SAVED: &str = "Cupola couldn't save this preview";

tokio::task_local! {
    static RETRY_LATER: std::cell::Cell<bool>;
}

/// A preview missing for a reason that can pass (no preview storage yet, a
/// failed save). A history refresh that met one must not mark the provider's
/// history as current, or the next refresh would skip retrying it.
fn transient(reason: &'static str) -> &'static str {
    let _ = RETRY_LATER.try_with(|retry| retry.set(true));
    reason
}

/// `normalize`, also saying whether a preview should be retried later.
pub(super) async fn normalize_for_refresh(
    state: &AppState,
    conversation: &str,
    turn: &str,
    item: &Value,
) -> Result<(Value, bool), String> {
    RETRY_LATER
        .scope(std::cell::Cell::new(false), async {
            let item = normalize(state, conversation, turn, item).await?;
            Ok((item, RETRY_LATER.with(std::cell::Cell::get)))
        })
        .await
}

/// Persist verified bytes under a stable ID derived from `id_seed` and the
/// content, record the conversation file, and return its artifact reference.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store(
    state: &AppState,
    conversation: &str,
    storage: &str,
    id_seed: &str,
    source_id: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<Value, String> {
    let owned = bytes.to_vec();
    let digest = tokio::task::spawn_blocking(move || hex::encode(Sha256::digest(owned)))
        .await
        .map_err(|e| e.to_string())?;
    store_hashed(
        state,
        conversation,
        storage,
        id_seed,
        source_id,
        name,
        mime,
        bytes,
        &digest,
    )
    .await
}

/// [`store`] with the bytes' SHA-256 already known.
#[allow(clippy::too_many_arguments)]
async fn store_hashed(
    state: &AppState,
    conversation: &str,
    storage: &str,
    id_seed: &str,
    source_id: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
    digest: &str,
) -> Result<Value, String> {
    let id = media_id(id_seed, mime, digest);
    let target = attachment_path(storage, &id, true)
        .await
        .ok_or("Preview storage unavailable")?;
    if target.exists() {
        if tokio::fs::read(&target).await.map_err(|e| e.to_string())? != bytes {
            return Err("Stored preview failed integrity verification".into());
        }
    } else {
        // Publish only complete bytes. A crash cannot poison the stable ID
        // with a partial file and permanently prevent inbox replay; its
        // temporary file is one the orphan prune recognizes.
        crate::attachments::write_immutable(&target, bytes).await?;
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated_at = state
        .store
        .upsert_conversation_file(
            &id,
            conversation,
            "attachment",
            name,
            Some(mime),
            Some(bytes.len() as i64),
            Some(digest),
            Some(&attachment_relative_path(&id)),
            "available",
            None,
            None,
            Some(source_id),
            &now,
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut reference = json!({"type":"wonderArtifact","file":{"id":id,"name":name,"mimeType":mime,
        "byteSize":bytes.len(),"sha256":digest,"state":"available","updatedAt":updated_at}});
    if let Some((width, height)) = pixel_size(bytes, mime) {
        reference["file"]["width"] = json!(width);
        reference["file"]["height"] = json!(height);
    }
    Ok(reference)
}

/// A conversation file's stable ID: its place in the thread and its content
/// (`digest`, the hex SHA-256 of its bytes).
fn media_id(id_seed: &str, mime: &str, digest: &str) -> String {
    deterministic_uuid(&format!("{id_seed}:{mime}:{digest}"))
}

/// Records a picture kept by reference (see `media_refs`) and returns its
/// artifact, which looks to the phone exactly like a stored copy's.
#[allow(clippy::too_many_arguments)]
async fn reference(
    state: &AppState,
    conversation: &str,
    id: &str,
    source_id: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
    digest: &str,
    location: MediaSourceLocation,
) -> Result<Value, String> {
    let size = pixel_size(bytes, mime);
    let source = wonder_store::MediaSource {
        location,
        width: size.map(|(width, _)| width),
        height: size.map(|(_, height)| height),
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated_at = state
        .store
        .upsert_referenced_file(
            id,
            conversation,
            name,
            mime,
            bytes.len() as i64,
            digest,
            source_id,
            &source,
            &now,
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut reference = json!({"type":"wonderArtifact","file":{"id":id,"name":name,"mimeType":mime,
        "byteSize":bytes.len(),"sha256":digest,"state":"available","updatedAt":updated_at}});
    if let Some((width, height)) = size {
        reference["file"]["width"] = json!(width);
        reference["file"]["height"] = json!(height);
    }
    Ok(reference)
}

/// Gives a picture kept by reference before history kept previews its
/// retained preview, made from the original while it still matches its
/// recorded size and hash. Costs a folder listing once it has one.
async fn retain_recorded(
    state: &AppState,
    conversation: &str,
    file: &wonder_store::StoredConversationFile,
) {
    let Some(media) = crate::projects::media_workspace(state, conversation).await else {
        return;
    };
    let Ok(Some(source)) = state.store.media_source(&file.id).await else {
        return;
    };
    let (id, size, sha) = (file.id.clone(), file.byte_size, file.sha256.clone());
    let _ = tokio::task::spawn_blocking(move || {
        let media = std::path::PathBuf::from(media);
        if media_refs::has_retained(&media, &id) {
            return;
        }
        if let Some(bytes) = media_refs::read_source(&source.location, size, sha.as_deref()) {
            media_refs::retain(&media, &id, &bytes);
        }
    })
    .await;
}

/// The Claude transcript this Project thread's pictures come from.
async fn transcript_of(state: &AppState, conversation: &str) -> Option<std::path::PathBuf> {
    let stored = state
        .store
        .project_conversation(conversation)
        .await
        .ok()??;
    if stored.family != wonder_store::AgentFamily::Claude {
        return None;
    }
    crate::desktop_activity::claude_session_transcript(
        stored.native_session_id.as_deref()?,
        &stored.cwd,
    )
}

/// A file's pixel size: from its reference, or read from its stored copy.
async fn known_size(
    state: &AppState,
    conversation: &str,
    file: &wonder_store::StoredConversationFile,
) -> Option<(u32, u32)> {
    if file.relative_path.is_none() {
        let source = state.store.media_source(&file.id).await.ok()??;
        return source.width.zip(source.height);
    }
    stored_size(state, conversation, file).await
}

/// A retained file's artifact reference, with its pixel size when known.
fn artifact(file: &wonder_store::StoredConversationFile, size: Option<(u32, u32)>) -> Value {
    let mut reference = json!({"type":"wonderArtifact","file":{"id":file.id,"name":file.name,
        "mimeType":file.mime_type,"byteSize":file.byte_size,"sha256":file.sha256,
        "state":file.state,"updatedAt":file.updated_at}});
    if let Some((width, height)) = size {
        reference["file"]["width"] = json!(width);
        reference["file"]["height"] = json!(height);
    }
    reference
}

/// An image's pixel size, read from its header, so the phone can lay out
/// the preview at its real proportions before the bytes arrive. SVG and
/// non-image media have none.
fn pixel_size(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    if !mime.starts_with("image/") || mime == "image/svg+xml" {
        return None;
    }
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// The pixel size of a file already retained for this conversation.
async fn stored_size(
    state: &AppState,
    conversation: &str,
    file: &wonder_store::StoredConversationFile,
) -> Option<(u32, u32)> {
    let mime = file.mime_type.clone()?;
    if !mime.starts_with("image/") || mime == "image/svg+xml" {
        return None;
    }
    let storage = crate::projects::media_workspace(state, conversation).await?;
    let path = attachment_path(&storage, &file.id, false).await?;
    // Retained files have no extension; the header names the format.
    tokio::task::spawn_blocking(move || {
        image::ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
    })
    .await
    .ok()
    .flatten()
}

/// The last component of a path on the Mac, when it is a usable file name.
fn file_name(path: &str) -> Option<String> {
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    (!name.is_empty() && name.len() <= 200 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

/// `hint` with the extension the verified type implies (from `fallback`), so
/// a misnamed file never claims another type.
fn named_like(hint: &str, fallback: &str) -> String {
    let Some((_, extension)) = fallback.rsplit_once('.') else {
        return hint.to_owned();
    };
    let path = std::path::Path::new(hint);
    let same = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        e.eq_ignore_ascii_case(extension) || (extension == "jpg" && e.eq_ignore_ascii_case("jpeg"))
    });
    if same {
        return hint.to_owned();
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("Tool result");
    format!("{stem}.{extension}")
}

/// Prompt images arrive as Claude base64 blocks, Codex `data:` URLs or Codex
/// local-image paths.
/// Store each one before path redaction as a verified
/// conversation file (same MIME, size and integrity checks as tool media) and
/// list them as the bubble's `attachmentIds`. A history refresh reuses the
/// stored file instead of decoding the image again.
async fn user_images(
    state: &AppState,
    conversation: &str,
    turn: &str,
    item_id: &str,
    mut item: Value,
) -> Result<Value, String> {
    let positions: Vec<usize> = item["content"]
        .as_array()
        .map(|content| {
            content
                .iter()
                .enumerate()
                .filter(|(_, block)| {
                    block["type"] == "localImage"
                        || (block["type"] == "image"
                            && (block["data"].is_string()
                                || block["unavailable"].is_string()
                                || block["url"]
                                    .as_str()
                                    .is_some_and(|u| u.starts_with("data:"))))
                })
                .map(|(position, _)| position)
                .collect()
        })
        .unwrap_or_default();
    if positions.is_empty() {
        return Ok(item);
    }
    let Some(storage) = crate::projects::media_workspace(state, conversation).await else {
        transient(NO_PREVIEW_STORAGE);
        return Ok(item);
    };
    let existing = state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    let mut total = 0;
    for (index, position) in positions.iter().enumerate() {
        let source_id = format!("user-media:{turn}:{item_id}:{index}");
        let block = &mut item["content"][*position];
        if let Some(file) = existing
            .iter()
            .find(|file| file.source_id.as_deref() == Some(&source_id) && file.state == "available")
        {
            ids.push(json!(file.id));
            let size = stored_size(state, conversation, file).await;
            *block = artifact(file, size);
            continue;
        }
        // An image the owner attached is never replaced by words in their
        // message; an unusable one keeps only the reason, without bytes.
        // Reading, decoding, checking and hashing run once, off the workers.
        let local = block["type"] == "localImage";
        let decoded = if local {
            let source = block["path"].as_str().unwrap_or("").to_owned();
            if private_elsewhere(&state.data_dir, Some(&storage), &source) {
                Err(PRIVATE_MEDIA.to_owned())
            } else {
                tokio::task::spawn_blocking(move || local_image(&source).map_err(str::to_owned))
                    .await
                    .map_err(|e| e.to_string())?
            }
        } else {
            match block["unavailable"].as_str() {
                Some(reason) => Err(reason.to_owned()),
                None => {
                    let raw = block.clone();
                    tokio::task::spawn_blocking(move || decode(&raw).map_err(str::to_owned))
                        .await
                        .map_err(|e| e.to_string())?
                }
            }
        };
        let checked = match decoded {
            Ok((bytes, _, _)) if total + bytes.len() > 16 * 1024 * 1024 => {
                Err("These images exceed the preview size limit".to_owned())
            }
            Ok((bytes, mime, _)) => {
                total += bytes.len();
                tokio::task::spawn_blocking(move || {
                    // The bounded file reader already verified a local image.
                    if !local {
                        validate(&bytes, &mime).map_err(str::to_owned)?;
                    }
                    let digest = hex::encode(Sha256::digest(&bytes));
                    Ok((bytes, mime, digest))
                })
                .await
                .map_err(|e| e.to_string())?
            }
            Err(reason) => Err(reason),
        };
        let (bytes, mime, digest) = match checked {
            Ok(file) => file,
            Err(reason) => {
                *block = json!({"type":"imageUnavailable","reason":reason});
                continue;
            }
        };
        let extension = mime.trim_start_matches("image/").replace("jpeg", "jpg");
        let name = if positions.len() == 1 {
            format!("Image.{extension}")
        } else {
            format!("Image {}.{extension}", index + 1)
        };
        let id_seed = format!("user-media:{conversation}:{turn}:{item_id}:{index}");
        let Ok(file) = store_hashed(
            state,
            conversation,
            &storage,
            &id_seed,
            &source_id,
            &name,
            &mime,
            &bytes,
            &digest,
        )
        .await
        else {
            *block = json!({"type":"imageUnavailable","reason":transient(PREVIEW_NOT_SAVED)});
            continue;
        };
        ids.push(file["file"]["id"].clone());
        item["content"][*position] = file;
    }
    item["attachmentIds"] = Value::Array(ids);
    Ok(item)
}
/// A file T3 Code saved for a message (`t3_attachments`).
struct T3File {
    name: String,
    path: String,
}

/// One `[Attached image "a.png" is saved at: <path>]` note in a message's text.
struct T3Note<'a> {
    image: bool,
    name: &'a str,
    path: &'a str,
    /// Where the note starts and ends in the text.
    span: std::ops::Range<usize>,
}

/// The notes T3 Code adds to the agent's message for each file the owner
/// attached there, in order.
fn t3_notes(text: &str) -> Vec<T3Note<'_>> {
    const OPEN: &str = "[Attached ";
    let mut notes = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(OPEN) {
        let start = from + found;
        from = start + OPEN.len();
        let rest = &text[from..];
        let (image, rest) = match (rest.strip_prefix("image \""), rest.strip_prefix("file \"")) {
            (Some(rest), _) => (true, rest),
            (_, Some(rest)) => (false, rest),
            _ => continue,
        };
        let Some((name, rest)) = rest.split_once("\" is saved at: ") else {
            continue;
        };
        let Some((path, _)) = rest.split_once(']') else {
            continue;
        };
        if name.contains('\n') || path.contains('\n') || path.is_empty() {
            continue;
        }
        let end = text.len() - rest.len() + path.len() + 1;
        notes.push(T3Note {
            image,
            name,
            path,
            span: start..end,
        });
        from = end;
    }
    notes
}

/// A message's files, typed in T3 Code or sent by another client, reach the
/// agent as a note per file, so a bubble showed `[Attached image "image.png"
/// is saved at: <path>]` instead of the picture. Each note naming a file in T3
/// Code's own attachments folder (`dir`; a note naming any other path is only
/// words) leaves the message's text, and its picture joins the message as a
/// local image (`user_images` stores it like any prompt image). Other files
/// are returned for `user_files`.
fn t3_attachments(dir: Option<&std::path::Path>, item: &mut Value) -> Vec<T3File> {
    let Some(dir) = dir.and_then(|dir| crate::canonical_path::canonicalize(dir).ok()) else {
        return Vec::new();
    };
    let Some(content) = item["content"].as_array_mut() else {
        return Vec::new();
    };
    let (mut images, mut files) = (Vec::new(), Vec::new());
    // Which notes left the text, in order, so a record of the message can
    // still be told from a message that never carried a file.
    let mut taken = Vec::new();
    for block in content.iter_mut() {
        let Some(text) = block["text"].as_str().filter(|_| block["type"] == "text") else {
            continue;
        };
        let mut kept = String::new();
        let mut last = 0;
        let mut found = false;
        for note in t3_notes(text) {
            let inside = std::path::Path::new(note.path).is_absolute()
                && crate::canonical_path::canonicalize(note.path)
                    .is_ok_and(|path| path.starts_with(&dir));
            if !inside {
                continue;
            }
            found = true;
            kept.push_str(text[last..note.span.start].trim_end());
            last = note.span.end;
            taken.push(
                json!({"kind": if note.image { "image" } else { "file" }, "name": note.name}),
            );
            if note.image {
                images.push(json!({"type": "localImage", "path": note.path}));
            } else {
                files.push(T3File {
                    name: note.name.to_owned(),
                    path: note.path.to_owned(),
                });
            }
        }
        if found {
            kept.push_str(&text[last..]);
            block["text"] = kept.trim().into();
        }
    }
    if images.is_empty() && files.is_empty() {
        return files;
    }
    content.retain(|block| !(block["type"] == "text" && block["text"].as_str() == Some("")));
    content.extend(images);
    item[T3_NOTES_TAKEN] = Value::Array(taken);
    files
}

/// The key of a message's item that lists the T3 Code notes taken out of its
/// text (`t3_attachments`): `{kind, name}` each, in order.
pub(crate) const T3_NOTES_TAKEN: &str = "t3NotesTaken";

/// A file T3 Code saved for a message, as a file card: stored as a verified
/// conversation file like any attachment, and listed in the bubble's
/// `attachmentIds`. One that is gone keeps only its reason.
async fn user_files(
    state: &AppState,
    conversation: &str,
    turn: &str,
    item_id: &str,
    item: &mut Value,
    files: Vec<T3File>,
) -> Result<(), String> {
    let Some(storage) = crate::projects::media_workspace(state, conversation).await else {
        transient(NO_PREVIEW_STORAGE);
        return Ok(());
    };
    let existing = state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let mut ids = item["attachmentIds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut blocks = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let source_id = format!("user-file:{turn}:{item_id}:{index}");
        if let Some(stored) = existing.iter().find(|stored| {
            stored.source_id.as_deref() == Some(&source_id) && stored.state == "available"
        }) {
            ids.push(json!(stored.id));
            blocks.push(artifact(stored, None));
            continue;
        }
        let path = file.path.clone();
        let read = tokio::task::spawn_blocking(move || local_file(&path))
            .await
            .map_err(|e| e.to_string())?;
        let (bytes, mime) = match read {
            Ok(read) => read,
            Err(reason) => {
                blocks.push(json!({"type": "imageUnavailable", "reason": format!("{}: {reason}", file.name)}));
                continue;
            }
        };
        let digest = hex::encode(Sha256::digest(&bytes));
        let id_seed = format!("user-file:{conversation}:{turn}:{item_id}:{index}");
        match store_hashed(
            state,
            conversation,
            &storage,
            &id_seed,
            &source_id,
            &file.name,
            &mime,
            &bytes,
            &digest,
        )
        .await
        {
            Ok(stored) => {
                ids.push(stored["file"]["id"].clone());
                blocks.push(stored);
            }
            Err(_) => blocks
                .push(json!({"type": "imageUnavailable", "reason": transient(PREVIEW_NOT_SAVED)})),
        }
    }
    if let Some(content) = item["content"].as_array_mut() {
        content.extend(blocks);
    }
    item["attachmentIds"] = Value::Array(ids);
    Ok(())
}

/// A regular file's bytes (at most `MAX_ATTACHMENT_BYTES`) and the type its
/// name and contents allow: a PDF or text file keeps its type, anything
/// else is a plain download.
fn local_file(path: &str) -> Result<(Vec<u8>, String), &'static str> {
    let path =
        crate::canonical_path::canonicalize(path).map_err(|_| "no longer on your computer")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A named pipe must not hold a history refresh waiting for a writer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(|_| "couldn't be opened")?;
    let metadata = file.metadata().map_err(|_| "couldn't be checked")?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err("isn't a regular file or is larger than 8 MB");
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ATTACHMENT_BYTES as u64 + 1),
        &mut bytes,
    )
    .map_err(|_| "couldn't be read")?;
    if bytes.is_empty() || bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err("is empty or larger than 8 MB");
    }
    let named = crate::filesystem::mime_for_path(&path);
    let mime = match named.as_deref() {
        Some("application/pdf") if bytes.starts_with(b"%PDF-") => "application/pdf",
        Some(text) if text.starts_with("text/") && std::str::from_utf8(&bytes).is_ok() => {
            if text == "text/html" {
                "text/plain"
            } else {
                text
            }
        }
        _ => "application/octet-stream",
    };
    Ok((bytes, mime.to_owned()))
}

const PRIVATE_MEDIA: &str = "This picture is in Cupola's private storage for another chat";

/// Whether `path` (absolute, or a `file://` URL) is another chat's stored
/// pictures and files (`project-media` outside `own`, this chat's media) or
/// the preview cache. Only those folders are refused, not the rest of
/// Cupola's data folder. Unresolvable paths are left to the reader.
fn private_elsewhere(data_dir: &std::path::Path, own: Option<&str>, path: &str) -> bool {
    let path = match path.strip_prefix("file://") {
        Some(url) => match media_refs::percent_decode(url) {
            Some(path) => path,
            None => return false,
        },
        None => path.to_owned(),
    };
    let (Ok(path), Ok(data)) = (
        crate::canonical_path::canonicalize(path),
        crate::canonical_path::canonicalize(data_dir),
    ) else {
        return false;
    };
    let own = own.and_then(|own| crate::canonical_path::canonicalize(own).ok());
    path.starts_with(data.join("media-cache"))
        || (path.starts_with(data.join("project-media"))
            && !own.is_some_and(|own| path.starts_with(own)))
}

/// The harness selected this exact image for the chat. Project file-browser
/// roots do not restrict previews of images already in its native transcript,
/// but Cupola's own storage of other chats does (`private_elsewhere`).
/// Retain only bounded, validated bytes; clients never receive a path reader.
fn local_image(path: &str) -> Result<(Vec<u8>, String, String), &'static str> {
    if !std::path::Path::new(path).is_absolute() {
        return Err("The image location is invalid");
    }
    let path = crate::canonical_path::canonicalize(path)
        .map_err(|_| "The image is no longer available on your computer")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A named pipe must not hold a history refresh waiting for a writer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|_| "The image could not be opened")?;
    let metadata = file
        .metadata()
        .map_err(|_| "The image could not be checked")?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err("Image is not a regular file or is larger than 8 MB");
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ATTACHMENT_BYTES as u64 + 1),
        &mut bytes,
    )
    .map_err(|_| "The image could not be read")?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err("Image is larger than 8 MB");
    }
    let svg = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    let mime = if svg {
        "image/svg+xml".to_owned()
    } else {
        image::guess_format(&bytes)
            .map_err(|_| "Invalid image")?
            .to_mime_type()
            .to_owned()
    };
    validate(&bytes, &mime)?;
    Ok((
        bytes,
        mime,
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    ))
}

fn unavailable(reason: &str) -> Value {
    json!({"type":"text","text":format!("Preview unavailable: {reason}.")})
}

/// `html_render` or `html_preview` of T3 Code's `t3-code` MCP server. Claude
/// names a call `mcp__t3-code__html_render`; Codex reports the server and the
/// tool apart.
fn t3_page_tool(item: &Value) -> Option<&'static str> {
    if item.get("type").and_then(Value::as_str) != Some("mcpToolCall") {
        return None;
    }
    let tool = item.get("tool").and_then(Value::as_str)?;
    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
    let name = ["mcp__t3-code__", "mcp__t3_code__"]
        .iter()
        .find_map(|prefix| tool.strip_prefix(prefix))
        .or_else(|| matches!(server, "t3-code" | "t3_code" | "t3code").then_some(tool))?;
    ["html_render", "html_preview"]
        .into_iter()
        .find(|known| *known == name)
}

/// The `htmlRender` record a finished call returned: T3 Code puts it in the
/// result's text (Claude) or structured content (Codex).
fn render_record(item: &Value) -> Option<Value> {
    result_record(item, "htmlRender")
}

/// The first object a finished call's result holds under `key`, or that
/// carries `key` itself (`visualId`), in structured content or JSON text.
fn result_record(item: &Value, key: &str) -> Option<Value> {
    fn find(value: &Value, key: &str, depth: usize) -> Option<Value> {
        if depth > 6 {
            return None;
        }
        match value {
            Value::Object(fields) => {
                if let Some(record) = fields.get(key) {
                    if record.is_object() {
                        return Some(record.clone());
                    }
                    if record.is_string() {
                        return Some(value.clone());
                    }
                }
                ["structuredContent", "content", "contentItems", "text"]
                    .iter()
                    .filter_map(|field| fields.get(*field))
                    .find_map(|value| find(value, key, depth + 1))
            }
            Value::Array(values) => values.iter().take(16).find_map(|v| find(v, key, depth + 1)),
            Value::String(text) if text.len() <= 64 * 1024 && text.contains(key) => {
                find(&serde_json::from_str(text).ok()?, key, depth + 1)
            }
            _ => None,
        }
    }
    ["result", "contentItems", "output"]
        .iter()
        .filter_map(|field| item.get(*field))
        .find_map(|value| find(value, key, 0))
}

/// A call of Cupola's own `wonder_show_visual`: a dynamic tool call (Codex,
/// and Claude through its bridge) or the ACP bridge's `Cupola` MCP call.
fn is_show_visual(item: &Value) -> bool {
    let tool = item.get("tool").and_then(Value::as_str);
    match item.get("type").and_then(Value::as_str) {
        Some("dynamicToolCall") => tool == Some(crate::thread_tools::SHOW_VISUAL),
        Some("mcpToolCall") => {
            tool == Some(crate::thread_tools::SHOW_VISUAL)
                && item.get("server").and_then(Value::as_str) == Some("Cupola")
        }
        _ => false,
    }
}

/// The visual a finished `wonder_show_visual` call kept: its file must be
/// one of this conversation's visuals, so a result can only name a visual
/// Cupola made here.
async fn shown_visual(
    state: &AppState,
    conversation: &str,
    item: &Value,
) -> Result<Option<Value>, String> {
    if item.get("status").and_then(Value::as_str) != Some("completed")
        || item.get("success").and_then(Value::as_bool) == Some(false)
    {
        return Ok(None);
    }
    let Some(record) = result_record(item, "visualId") else {
        return Ok(None);
    };
    let Some(id) = record.get("visualId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let files = state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(|e| e.to_string())?;
    let Some(file) = files
        .iter()
        .find(|file| file.id == id && crate::thread_tools::visual::is_visual(file))
    else {
        return Ok(None);
    };
    let mut page = json!({
        "title": page_title(record.get("title").and_then(Value::as_str)),
        "file": artifact(file, None)["file"],
    });
    if let Some(replaces) = record
        .get("replaces")
        .and_then(Value::as_str)
        .filter(|replaced| {
            files
                .iter()
                .any(|file| file.id == *replaced && crate::thread_tools::visual::is_visual(file))
        })
    {
        page["replaces"] = json!(replaces);
    }
    Ok(Some(page))
}

/// A T3 attachment ID names one file in T3's attachments folder: letters,
/// digits, `-` and `_` only, so it can never name another folder or file.
fn t3_attachment_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

/// The final page T3 Code shows for `id`: `<id>.html` inside `folder`, a
/// regular file whose real location (after links) is still inside it.
fn t3_page(folder: &std::path::Path, id: &str) -> Option<Vec<u8>> {
    let id = t3_attachment_id(id)?;
    let folder = crate::canonical_path::canonicalize(folder).ok()?;
    let path = crate::canonical_path::canonicalize(folder.join(format!("{id}.html"))).ok()?;
    if path.parent() != Some(folder.as_path()) {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_ATTACHMENT_BYTES as u64 + 1),
        &mut bytes,
    )
    .ok()?;
    (!bytes.is_empty()
        && bytes.len() <= MAX_ATTACHMENT_BYTES
        && std::str::from_utf8(&bytes).is_ok())
    .then_some(bytes)
}

/// A page title fit for a file name and a heading.
pub(crate) fn page_title(raw: Option<&str>) -> String {
    let title: String = raw
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect();
    if title.is_empty() {
        "Page".into()
    } else {
        title
    }
}

/// Keeps the page an `html_render` call published as a verified `text/html`
/// conversation file and returns what the phone needs to show it: the file,
/// the title and the heights T3 measured. The final page (with T3's theme
/// bootstrap) is read from T3 Code's attachments; without it, the HTML the
/// agent sent. A page that can't be kept says why.
async fn html_render(
    state: &AppState,
    conversation: &str,
    turn: &str,
    item_id: &str,
    item: &Value,
) -> Result<Value, String> {
    // Only a finished call names its page; until then there is nothing to keep.
    let Some(record) = render_record(item) else {
        return Ok(Value::Null);
    };
    let title = page_title(
        record
            .get("title")
            .or_else(|| item.pointer("/arguments/title"))
            .and_then(Value::as_str),
    );
    let mut page = json!({"title": title});
    if let Some(height) = record
        .get("height")
        .or_else(|| item.pointer("/arguments/height"))
        .and_then(Value::as_u64)
        .filter(|h| (1..=100_000).contains(h))
    {
        page["height"] = json!(height);
    }
    if let Some(heights) = record.get("heights").and_then(Value::as_array) {
        let pairs: Vec<Value> = heights
            .iter()
            .take(16)
            .filter_map(|pair| {
                let width = pair.get(0)?.as_u64().filter(|w| (1..=10_000).contains(w))?;
                let height = pair
                    .get(1)?
                    .as_u64()
                    .filter(|h| (1..=1_000_000).contains(h))?;
                Some(json!([width, height]))
            })
            .collect();
        if !pairs.is_empty() {
            page["heights"] = Value::Array(pairs);
        }
    }
    let source_id = format!("tool-media:{turn}:{item_id}:page");
    let files = state
        .store
        .list_conversation_files(conversation)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(file) = files
        .iter()
        .find(|file| file.source_id.as_deref() == Some(&source_id) && file.state == "available")
    {
        page["file"] = artifact(file, None)["file"].clone();
        return Ok(page);
    }
    let folder = state.t3_code.attachments_dir();
    let attachment = record
        .get("attachmentId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let sent = item
        .pointer("/arguments/html")
        .and_then(Value::as_str)
        .filter(|html| !html.trim().is_empty() && html.len() <= MAX_ATTACHMENT_BYTES)
        .map(|html| html.as_bytes().to_vec());
    let bytes = tokio::task::spawn_blocking(move || {
        folder
            .zip(attachment)
            .and_then(|(folder, id)| t3_page(&folder, &id))
            .or(sent)
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(bytes) = bytes else {
        page["unavailable"] = json!("The page is no longer on your computer");
        return Ok(page);
    };
    let Some(storage) = crate::projects::media_workspace(state, conversation).await else {
        page["unavailable"] = json!(transient(NO_PREVIEW_STORAGE));
        return Ok(page);
    };
    let id_seed = format!("tool-media:{conversation}:{turn}:{item_id}:page");
    match store(
        state,
        conversation,
        &storage,
        &id_seed,
        &source_id,
        &format!("{title}.html"),
        "text/html",
        &bytes,
    )
    .await
    {
        Ok(reference) => page["file"] = reference["file"].clone(),
        Err(_) => page["unavailable"] = json!(transient(PREVIEW_NOT_SAVED)),
    }
    Ok(page)
}
fn collect(value: &Value, path: Vec<String>, paths: &mut Vec<Vec<String>>, depth: usize) {
    if depth > 12 {
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let mut next = path.clone();
                next.push(index.to_string());
                collect(value, next, paths, depth + 1);
            }
        }
        Value::Object(fields) => {
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some(
                    "image" | "audio" | "inputImage" | "input_image" | "resource" | "resource_link"
                )
            ) {
                paths.push(path);
                return;
            }
            for key in ["content", "contentItems"] {
                if let Some(value) = fields.get(key) {
                    let mut next = path.clone();
                    next.push(key.into());
                    collect(value, next, paths, depth + 1);
                }
            }
        }
        _ => {}
    }
}
fn at_mut<'a>(value: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let Some((first, rest)) = path.split_first() else {
        return Some(value);
    };
    let next = match value {
        Value::Array(values) => values.get_mut(first.parse::<usize>().ok()?)?,
        _ => value.get_mut(first)?,
    };
    at_mut(next, rest)
}
fn decode(value: &Value) -> Result<(Vec<u8>, String, String), &'static str> {
    let resource = value.get("resource").unwrap_or(value);
    let mut mime = resource
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut encoded = resource
        .get("data")
        .or_else(|| resource.get("blob"))
        .and_then(Value::as_str);
    if encoded.is_none() {
        // Cupola sends attached images to Codex as `data:` URLs; an external
        // URL stays inert.
        if let Some(url) = resource
            .get("imageUrl")
            .or_else(|| resource.get("image_url"))
            .or_else(|| {
                resource
                    .get("url")
                    .filter(|url| url.as_str().is_some_and(|url| url.starts_with("data:")))
            })
            .and_then(Value::as_str)
        {
            let (header, body) = url
                .strip_prefix("data:")
                .and_then(|s| s.split_once(";base64,"))
                .ok_or("Ask the Project to attach this image as a file")?;
            mime = header.into();
            encoded = Some(body);
        }
    }
    let bytes = if let Some(encoded) = encoded {
        if encoded.len() > MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 {
            return Err("File is larger than 8 MB");
        }
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid encoded file")?
    } else if let Some(text) = resource.get("text").and_then(Value::as_str) {
        if text.len() > MAX_ATTACHMENT_BYTES {
            return Err("File is larger than 8 MB");
        }
        if mime.is_empty() {
            mime = "text/plain".into();
        }
        text.as_bytes().to_vec()
    } else {
        return Err("Ask the Project to attach this resource as a file");
    };
    if bytes.is_empty() || bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err("File is empty or larger than 8 MB");
    }
    if mime.is_empty() && value.get("type").and_then(Value::as_str) == Some("image") {
        mime = image::guess_format(&bytes)
            .map_err(|_| "Invalid image")?
            .to_mime_type()
            .to_owned();
    }
    let extension = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/mp4" => "m4a",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return Err("This file type is not supported"),
    };
    Ok((bytes, mime, format!("Tool result.{extension}")))
}
/// An SVG the phone draws in a sandbox (no scripts, no network): UTF-8 with
/// an `<svg` root and no document type or entity declarations.
fn valid_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    lower.contains("<svg") && !lower.contains("<!doctype") && !lower.contains("<!entity")
}

/// Audio and video by their leading bytes, as media previews check them.
fn media_matches(bytes: &[u8], mime: &str) -> bool {
    let ftyp = bytes.get(4..8) == Some(b"ftyp");
    match mime {
        "video/mp4" | "video/quicktime" | "audio/mp4" => ftyp,
        "audio/mpeg" => {
            bytes.starts_with(b"ID3")
                || bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0
        }
        "audio/wav" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE"),
        _ => false,
    }
}

fn validate(bytes: &[u8], mime: &str) -> Result<(), &'static str> {
    if mime == "image/svg+xml" {
        if !valid_svg(bytes) {
            return Err("Invalid SVG image");
        }
    } else if mime.starts_with("audio/") || mime.starts_with("video/") {
        if !media_matches(bytes, mime) {
            return Err("Media type does not match its contents");
        }
    } else if mime.starts_with("image/") {
        let format = image::guess_format(bytes).map_err(|_| "Invalid image")?;
        if format.to_mime_type() != mime {
            return Err("Image type does not match its contents");
        }
        let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        reader
            .decode()
            .map_err(|_| "Image is damaged or exceeds preview limits")?;
    } else if mime == "application/pdf" {
        if artifact_mime_type("result.pdf", bytes) != Some(mime) {
            return Err("Invalid PDF");
        }
    } else if std::str::from_utf8(bytes).is_err() {
        return Err("Invalid text file");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn images_the_owner_attached_on_the_mac_become_verified_bubble_attachments() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(32, 32)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        // The bridge's projection of a Claude Code prompt with two images, one
        // too large to forward, plus a block that claims to be a PNG.
        let raw = json!({"type":"userMessage","id":"prompt","content":[
            {"type":"text","text":"What changed in [Image #1]?"},
            {"type":"image","mimeType":"image/png","data":data},
            {"type":"image","mimeType":"image/png","unavailable":"This image is larger than 8 MB"},
            {"type":"image","mimeType":"image/png","data":"bm90IGFuIGltYWdl"},
        ]});
        let first = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        let ids = first["attachmentIds"].as_array().unwrap().clone();
        assert_eq!(ids.len(), 1, "{first}");
        let file = &first["content"][1]["file"];
        assert_eq!(file["id"], ids[0]);
        assert_eq!(file["mimeType"], "image/png");
        assert_eq!(file["sha256"], hex::encode(Sha256::digest(&bytes)));
        assert_eq!(first["content"][2]["type"], "imageUnavailable");
        assert_eq!(first["content"][3]["type"], "imageUnavailable");
        assert!(
            first.to_string().len() < 2048,
            "No base64 reaches projection"
        );
        // Words stay the owner's; nothing is appended to them.
        assert_eq!(
            crate::extract_text(&first).as_deref(),
            Some("What changed in [Image #1]?")
        );
        let stored = state
            .store
            .list_conversation_files("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .into_iter()
            .find(|f| Some(f.id.as_str()) == ids[0].as_str())
            .unwrap();
        assert_eq!(stored.name, "Image 1.png");
        assert_eq!(stored.byte_size, Some(bytes.len() as i64));
        // A history refresh resolves to the same file.
        let again = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        assert_eq!(again["attachmentIds"], first["attachmentIds"]);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn codex_prompt_images_survive_redaction_and_source_removal() {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let (_fixture, mut state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        state.denied_roots = vec![dir.path().to_string_lossy().into_owned()];
        // A desktop attachment can live outside both Project and temp folders.
        let path = dir.path().join("pasted-image.jpg");
        image::DynamicImage::new_rgb8(32, 32).save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let raw = json!({"type":"userMessage","id":"codex-prompt","content":[
            {"type":"text","text":"What changed in this screenshot?"},
            {"type":"localImage","path":path,"detail":null}
        ]});
        let first = normalize(&state, conversation, "turn", &raw).await.unwrap();
        let id = first["attachmentIds"][0]
            .as_str()
            .expect("bubble attachment");
        let file = &first["content"][1]["file"];
        assert_eq!(file["id"], id);
        assert_eq!(file["mimeType"], "image/jpeg");
        assert_eq!(file["sha256"], hex::encode(Sha256::digest(&bytes)));
        assert_eq!(first["content"][0], raw["content"][0]);
        assert!(!first.to_string().contains(path.to_str().unwrap()));
        let detail = thread_item_upsert_detail("turn", &first, "inProgress", None).unwrap();
        let projected: Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(projected["item"]["attachmentIds"][0], id);
        assert_eq!(projected["item"]["content"][1]["file"]["id"], id);
        std::fs::remove_file(&path).unwrap();
        let replay = normalize(&state, conversation, "turn", &raw).await.unwrap();
        assert_eq!(
            replay, first,
            "refresh must reuse retained bytes after temp cleanup"
        );
        assert_eq!(
            normalize(&state, conversation, "turn", &first)
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            state
                .store
                .list_conversation_files(conversation)
                .await
                .unwrap()
                .len(),
            1
        );
        let uri = format!("/api/v1/conversations/{conversation}/files/{id}");
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(&uri)
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        assert_eq!(
            to_bytes(response.into_body(), MAX_ATTACHMENT_BYTES)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        let denied = router(state.clone())
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(denied.status(), StatusCode::OK);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    // Cupola sends attached images to Codex as verified `data:` URLs; the
    // echoed prompt keeps its preview as a stored file, without the bytes.
    #[tokio::test]
    async fn codex_prompt_data_url_images_become_stored_previews() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let raw = json!({"type":"userMessage","id":"cupola-prompt","content":[
            {"type":"text","text":"Look"}, {"type":"image","url":url}
        ]});
        let normalized = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        let file = &normalized["content"][1]["file"];
        assert_eq!(normalized["attachmentIds"][0], file["id"]);
        assert_eq!(file["sha256"], hex::encode(Sha256::digest(&png)));
        assert!(!normalized.to_string().contains("base64,"));
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unusable_codex_prompt_images_keep_text_and_never_expose_paths() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let path = dir.path().join("private.png");
        std::fs::write(&path, b"not an image").unwrap();
        for (index, source) in [
            json!(path),
            json!(dir.path().join("missing.png")),
            Value::Null,
        ]
        .into_iter()
        .enumerate()
        {
            let raw = json!({"type":"userMessage","id":format!("bad-prompt-{index}"),"content":[
                {"type":"text","text":"Please inspect this"}, {"type":"localImage","path":source}
            ]});
            let normalized =
                normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                    .await
                    .unwrap();
            assert_eq!(normalized["content"][0], raw["content"][0]);
            assert_eq!(normalized["content"][1]["type"], "imageUnavailable");
            assert!(normalized["content"][1].get("path").is_none());
            assert_eq!(normalized["attachmentIds"], json!([]));
        }
        assert!(state
            .store
            .list_conversation_files("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .is_empty());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_images_survive_projection_replay_and_authenticated_download() {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        // The preview is named after the file the generator wrote, with the
        // extension its verified type implies, and carries its pixel size.
        for (index, format, (width, height), name) in [
            (0, image::ImageFormat::Png, (160, 90), "generated.png"),
            (1, image::ImageFormat::Jpeg, (90, 160), "generated.jpg"),
        ] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(width, height)
                .write_to(&mut encoded, format)
                .unwrap();
            let bytes = encoded.into_inner();
            let mut raw = json!({"id":format!("generated-{index}"),"type":"imageGeneration","status":"completed",
                "result":base64::engine::general_purpose::STANDARD.encode(&bytes),
                "savedPath":"/private/not-readable/generated.png","failure":null});
            if index == 1 {
                raw["fileId"] = json!("also-has-inline-data");
            }
            let first = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                .await
                .unwrap();
            let file = &first["result"]["content"][0]["file"];
            assert_eq!(file["mimeType"], format.to_mime_type());
            assert_eq!(file["sha256"], hex::encode(Sha256::digest(&bytes)));
            assert_eq!(file["byteSize"], bytes.len());
            assert_eq!(file["name"], name);
            assert_eq!(
                (file["width"].clone(), file["height"].clone()),
                (json!(width), json!(height))
            );
            assert!(first.get("savedPath").is_none());
            assert!(!first.to_string().contains("not-readable"));
            let detail = thread_item_upsert_detail("turn", &first, "completed", None).unwrap();
            let projected: Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(
                projected["item"]["result"]["content"][0]["file"]["id"],
                file["id"]
            );
            assert!(!detail.contains(raw["result"].as_str().unwrap()));
            let replay = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                .await
                .unwrap();
            assert_eq!(replay["result"]["content"][0]["file"]["id"], file["id"]);
            assert_eq!(
                normalize(
                    &state,
                    "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                    "turn",
                    &first
                )
                .await
                .unwrap(),
                first
            );
            let uri = format!(
                "/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/files/{}",
                file["id"].as_str().unwrap()
            );
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], format.to_mime_type());
            assert_eq!(
                to_bytes(response.into_body(), MAX_ATTACHMENT_BYTES)
                    .await
                    .unwrap()
                    .as_ref(),
                bytes
            );
            let denied = router(state.clone())
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(denied.status(), StatusCode::OK);
        }
        assert_eq!(
            state
                .store
                .list_conversation_files("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn invalid_generated_images_are_inert_and_never_read_saved_paths() {
        let (dir, state) = crate::ingestion::tests::fixture().await;
        let path = dir.path().join("valid.png");
        image::DynamicImage::new_rgb8(2, 2).save(&path).unwrap();
        for result in [
            Value::Null,
            json!(""),
            json!("invalid base64"),
            json!(base64::engine::general_purpose::STANDARD.encode(b"not an image")),
            json!("A".repeat(MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 4)),
        ] {
            let raw = json!({"id":"bad","type":"imageGeneration","status":"completed","result":result,"savedPath":path});
            let normalized =
                normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                    .await
                    .unwrap();
            assert_eq!(normalized["result"]["content"][0]["type"], "text");
            assert!(normalized["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Preview unavailable:"));
            assert!(normalized.get("savedPath").is_none());
        }
        // 0.164.0 may return only an attachment-store identity, including
        // migration items with an empty result. A readable savedPath is still
        // untrusted; projection and replay must explain the missing preview.
        for result in [None, Some(Value::Null), Some(json!(""))] {
            let mut raw = json!({"id":"file-only","type":"imageGeneration","status":"completed",
                "fileId":"file-schema-proof","savedPath":path});
            if let Some(result) = result {
                raw["result"] = result;
            }
            let normalized =
                normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                    .await
                    .unwrap();
            assert_eq!(
                normalized["result"]["content"][0],
                json!({"type":"text",
                "text":"Preview unavailable: This Codex version keeps generated images on your computer; open it in Codex."})
            );
            assert!(normalized.get("savedPath").is_none());
            assert!(!normalized.to_string().contains(path.to_str().unwrap()));
            let detail = thread_item_upsert_detail("turn", &normalized, "completed", None).unwrap();
            let projected: Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(projected["item"]["result"], normalized["result"]);
            assert_eq!(
                normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                    .await
                    .unwrap(),
                normalized
            );
            assert_eq!(
                normalize(
                    &state,
                    "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                    "turn",
                    &normalized
                )
                .await
                .unwrap(),
                normalized
            );
        }
        for status in ["inProgress", "failed"] {
            let raw = json!({"id":"file-reference","type":"imageGeneration","status":status,
                "fileId":"file-schema-proof","result":""});
            assert_eq!(
                normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                    .await
                    .unwrap(),
                raw
            );
        }
        let pending =
            json!({"id":"pending","type":"imageGeneration","status":"inProgress","result":null});
        assert_eq!(
            normalize(
                &state,
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "turn",
                &pending
            )
            .await
            .unwrap(),
            pending
        );
        let failure = json!({"id":"failed","type":"imageGeneration","status":"failed","result":null,"failure":"Generation failed"});
        assert_eq!(
            normalize(
                &state,
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                "turn",
                &failure
            )
            .await
            .unwrap(),
            failure
        );
        assert!(state
            .store
            .list_conversation_files("d19b0f7e-97bb-45d7-8206-324a62386dd9")
            .await
            .unwrap()
            .is_empty());
    }

    // Contract (owner, Nov 6): the note T3 Code adds for each file a message
    // carried (`[Attached image "image.png" is saved at: <path>]`) is not
    // shown as words. A picture in T3 Code's attachments folder joins the
    // message as an image, another file there as a file card; a note naming
    // any other path stays as typed (it must not read an arbitrary file).
    #[test]
    fn t3_codes_attachment_notes_leave_the_text_and_become_files() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = dir.path().join("attachments");
        std::fs::create_dir(&attachments).unwrap();
        let (picture, notes) = (attachments.join("abc-1.png"), attachments.join("abc-2.txt"));
        image::DynamicImage::new_rgb8(3, 2).save(&picture).unwrap();
        std::fs::write(&notes, "hello").unwrap();
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let note = |kind: &str, name: &str, path: &std::path::Path| {
            format!(
                "[Attached {kind} \"{name}\" is saved at: {}]",
                path.display()
            )
        };
        let typed = note("image", "x.png", &outside);
        let text = format!(
            "Look at these\n\n{}\n\n{}\n\n{typed}",
            note("image", "image.png", &picture),
            note("file", "notes.txt", &notes)
        );
        let mut item = json!({"id": "u", "type": "userMessage",
            "content": [{"type": "text", "text": text}]});
        let files = t3_attachments(Some(&attachments), &mut item);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "notes.txt");
        assert_eq!(
            item["content"],
            json!([
                {"type": "text", "text": format!("Look at these\n\n{typed}")},
                {"type": "localImage", "path": picture}
            ]),
            "the picture joins the message; a note outside the folder stays words"
        );
        assert_eq!(
            item[T3_NOTES_TAKEN],
            json!([{"kind": "image", "name": "image.png"}, {"kind": "file", "name": "notes.txt"}]),
            "the item says which notes left its text, so pairing can tell it from plain words"
        );
        let (bytes, mime) = local_file(notes.to_str().unwrap()).unwrap();
        assert_eq!(
            (bytes.as_slice(), mime.as_str()),
            (&b"hello"[..], "text/plain")
        );
        // Only a note, and no folder to trust: nothing changes.
        let mut alone = json!({"id": "u", "type": "userMessage", "content": [
            {"type": "text", "text": note("image", "image.png", &picture)}]});
        let before = alone.clone();
        assert!(t3_attachments(None, &mut alone).is_empty());
        assert_eq!(alone, before);
        // Alone, the note leaves no empty text block.
        t3_attachments(Some(&attachments), &mut alone);
        assert_eq!(
            alone["content"],
            json!([{"type": "localImage", "path": picture}])
        );
        // Not a note: left alone.
        let words = "[Attached something] and [Attached image \"a\" is saved at: ]";
        assert!(t3_notes(words).is_empty());
    }

    #[tokio::test]
    async fn local_image_view_outside_project_roots_becomes_a_replay_safe_file() {
        let (_fixture, mut state) = crate::ingestion::tests::fixture().await;
        // The harness already selected this image. It can live outside both
        // the Project and temporary folders, including a legacy denied root.
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        state.denied_roots = vec![dir.path().to_string_lossy().into_owned()];
        let path = dir.path().join("screenshot.png");
        image::DynamicImage::new_rgb8(3, 2).save(&path).unwrap();
        let raw = json!({"id":"local-picture","type":"imageView","path":path});
        let normalized = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        assert_eq!(normalized["result"]["content"][0]["type"], "wonderArtifact");
        std::fs::remove_file(&path).unwrap();
        let replay = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        image::DynamicImage::new_rgb8(2, 2).save(&path).unwrap();
        assert_eq!(
            normalized["result"]["content"][0]["file"]["id"],
            replay["result"]["content"][0]["file"]["id"]
        );
        // Named after the file it shows, sized from the retained copy on replay.
        for file in [&normalized, &replay].map(|item| &item["result"]["content"][0]["file"]) {
            assert_eq!(file["name"], "screenshot.png");
            assert_eq!(
                (file["width"].as_u64(), file["height"].as_u64()),
                (Some(3), Some(2))
            );
        }
        assert!(local_image(path.to_str().unwrap()).is_ok());
        #[cfg(unix)]
        {
            let linked = dir.path().join("linked.png");
            std::os::unix::fs::symlink(&path, &linked).unwrap();
            assert!(local_image(linked.to_str().unwrap()).is_ok());
            let fifo = dir.path().join("pipe.png");
            let cpath = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
            assert!(local_image(fifo.to_str().unwrap()).is_err());
        }
        assert!(local_image(dir.path().to_str().unwrap()).is_err());
        assert!(local_image("relative.png").is_err());
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(local_image(path.to_str().unwrap()).is_err());
        std::fs::write(&path, b"not an image").unwrap();
        assert!(local_image(path.to_str().unwrap()).is_err());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Oct 25 audit: a picture an agent looked at was previewed even from
    /// another chat's stored media. Cupola's storage previews only the
    /// chat's own; the rest of the Mac is unchanged.
    #[tokio::test]
    async fn image_views_of_another_chats_stored_media_are_not_previewed() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let own = crate::projects::media_workspace(&state, conversation)
            .await
            .unwrap();
        let other = state
            .data_dir
            .join("project-media")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&other).unwrap();
        let theirs = other.join("photo.png");
        let mine = std::path::Path::new(&own).join("photo.png");
        for path in [&theirs, &mine] {
            image::DynamicImage::new_rgb8(3, 2).save(path).unwrap();
        }
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(std::fs::read(&theirs).unwrap());
        for raw in [
            json!({"id":"theirs","type":"imageView","path":theirs}),
            json!({"id":"theirs-inline","type":"imageView","path":format!("file://{}", theirs.display()),
                "result":{"content":[{"type":"image","mimeType":"image/png","data":encoded}]}}),
        ] {
            let item = normalize(&state, conversation, "turn", &raw).await.unwrap();
            let shown = &item["result"]["content"][0];
            assert_eq!(shown["type"], "text", "{item}");
            assert!(shown["text"].as_str().unwrap().contains("private storage"));
        }
        let raw = json!({"id":"mine","type":"imageView","path":mine});
        let item = normalize(&state, conversation, "turn", &raw).await.unwrap();
        assert_eq!(item["result"]["content"][0]["type"], "wonderArtifact");
        let prompt = json!({"type":"userMessage","id":"prompt","content":[
            {"type":"localImage","path":theirs},
        ]});
        let item = normalize(&state, conversation, "turn", &prompt)
            .await
            .unwrap();
        assert_eq!(item["content"][0]["type"], "imageUnavailable");
        assert_eq!(item["content"][0]["reason"], PRIVATE_MEDIA);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replay_keeps_one_verified_authenticated_file() {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        assert!(validate(&bytes, "image/jpeg").is_err());
        let content = json!([
            {"type":"text","text":"Result"},
            {"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(&bytes)}
        ]);
        let cases = [
            (
                json!({"id":"image-item","type":"mcpToolCall","result":{"content":content}}),
                "/result/content/1/file",
            ),
            (
                json!({"id":"acp-read","type":"mcpToolCall","contentItems":content}),
                "/contentItems/1/file",
            ),
            (
                json!({"id":"acp-command","type":"commandExecution","contentItems":content}),
                "/contentItems/1/file",
            ),
            (
                json!({"id":"acp-edit","type":"fileChange","contentItems":content}),
                "/contentItems/1/file",
            ),
        ];
        for (index, (raw, pointer)) in cases.into_iter().enumerate() {
            let first = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            let second = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
                .await
                .unwrap();
            assert_eq!(
                first, second,
                "unchanged image replay must preserve its timestamp and event payload"
            );
            let file = first.pointer(pointer).unwrap();
            assert_eq!(file["id"], second.pointer(pointer).unwrap()["id"]);
            assert_eq!(
                state
                    .store
                    .list_conversation_files("d19b0f7e-97bb-45d7-8206-324a62386dd9")
                    .await
                    .unwrap()
                    .len(),
                index + 1
            );
            assert!(!first.to_string().contains("\"data\""));
            let projected = crate::history::sanitize_typed_item(&first, None);
            assert_eq!(projected.pointer(pointer), Some(file));
            let uri = format!(
                "/api/v1/conversations/d19b0f7e-97bb-45d7-8206-324a62386dd9/files/{}",
                file["id"].as_str().unwrap()
            );
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header("x-wonder-loopback-capability", &state.loopback_capability)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), MAX_ATTACHMENT_BYTES)
                    .await
                    .unwrap()
                    .as_ref(),
                bytes
            );
            let denied = router(state.clone())
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(denied.status(), StatusCode::OK);
            // The reference resolves to the complete persisted bytes.
            let workspace =
                crate::projects::media_workspace(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9")
                    .await
                    .unwrap();
            let path = attachment_path(&workspace, file["id"].as_str().unwrap(), false)
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
        }
    }

    /// SVG drawings, audio and video a tool returns become verified files the
    /// phone shows inline; content that does not match its type stays inert.
    #[tokio::test]
    async fn tool_svg_audio_and_video_become_verified_files() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>"#;
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.resize(64, 0);
        let mut mp4 = b"\0\0\0\x18ftypmp42".to_vec();
        mp4.resize(64, 0);
        let content = json!([
            {"type":"resource","resource":{"uri":"file:///chart.svg","mimeType":"image/svg+xml","blob":b64(svg)}},
            {"type":"audio","mimeType":"audio/wav","data":b64(&wav)},
            {"type":"resource","resource":{"uri":"file:///clip.mp4","mimeType":"video/mp4","blob":b64(&mp4)}},
            {"type":"audio","mimeType":"audio/mpeg","data":b64(b"not an mp3 at all")},
            {"type":"resource","resource":{"mimeType":"image/svg+xml","text":"<!DOCTYPE svg [<!ENTITY x \"y\">]><svg>&x;</svg>"}},
            {"type":"audio","mimeType":"audio/ogg","data":b64(b"OggS")},
        ]);
        let raw = json!({"id":"media-item","type":"mcpToolCall","result":{"content":content}});
        let item = normalize(&state, "d19b0f7e-97bb-45d7-8206-324a62386dd9", "turn", &raw)
            .await
            .unwrap();
        let blocks = item["result"]["content"].as_array().unwrap();
        for (index, mime) in [(0, "image/svg+xml"), (1, "audio/wav"), (2, "video/mp4")] {
            assert_eq!(blocks[index]["type"], "wonderArtifact", "{index}: {item}");
            assert_eq!(blocks[index]["file"]["mimeType"], mime);
        }
        for (index, block) in blocks.iter().enumerate().skip(3) {
            assert_eq!(block["type"], "text", "{index}: {item}");
            assert!(block["text"]
                .as_str()
                .unwrap()
                .starts_with("Preview unavailable"));
        }
        assert!(
            item.to_string().len() < 4096,
            "No base64 reaches projection"
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// A noisy screenshot-sized PNG (it stays large, so a preview is smaller).
    fn screenshot(width: u32, height: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_fn(width, height, |x, y| {
            let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 7;
            image::Rgb([noise as u8, (noise >> 8) as u8, (x + y) as u8])
        })
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
        out.into_inner()
    }

    async fn fetch(
        state: &AppState,
        conversation: &str,
        id: &str,
        query: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/conversations/{conversation}/files/{id}{query}"
                    ))
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, MAX_ATTACHMENT_BYTES).await.unwrap().to_vec();
        (parts.status, parts.headers, bytes)
    }

    /// Bytes under the conversation's preview folder (copies Cupola keeps).
    fn kept_bytes(state: &AppState) -> u64 {
        fn walk(path: &std::path::Path) -> u64 {
            std::fs::read_dir(path).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|entry| {
                        let metadata = entry.metadata().unwrap();
                        if metadata.is_dir() {
                            walk(&entry.path())
                        } else {
                            metadata.len()
                        }
                    })
                    .sum()
            })
        }
        walk(&state.data_dir.join("project-media"))
    }

    /// Owner (Oct 22): Cupola copied every screenshot an agent viewed into
    /// project-media (815 MB). A Codex imageView names a file on the Mac; the
    /// preview now refers to it, is read and verified on demand, shrinks for
    /// the phone, and says so plainly once the file is gone or changed.
    #[tokio::test]
    async fn image_view_files_are_referenced_not_copied() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let path = dir.path().join("screen shot.png");
        let bytes = screenshot(1600, 1000);
        std::fs::write(&path, &bytes).unwrap();
        let before = kept_bytes(&state);
        // Codex names the file by path, or (from its own history) by file URL
        // with the bytes already inline.
        let url = format!(
            "file://{}",
            crate::canonical_path::canonicalize(&path)
                .unwrap()
                .to_str()
                .unwrap()
                .replace(' ', "%20")
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        for (index, raw) in [
            json!({"id":"view-path","type":"imageView","path":path}),
            json!({"id":"view-url","type":"imageView","path":url,
                "result":{"content":[{"type":"image","mimeType":"image/png","data":encoded}]}}),
        ]
        .into_iter()
        .enumerate()
        {
            let item = normalize(&state, conversation, "turn", &raw).await.unwrap();
            let file = item["result"]["content"][0]["file"].clone();
            assert_eq!(
                file["sha256"],
                hex::encode(Sha256::digest(&bytes)),
                "{index}"
            );
            assert_eq!(
                (file["width"].clone(), file["height"].clone()),
                (json!(1600), json!(1000))
            );
            let detail = thread_item_upsert_detail("turn", &item, "completed", None).unwrap();
            assert!(
                !detail.contains(dir.path().to_str().unwrap()),
                "the path stays on the Mac: {detail}"
            );
            let id = file["id"].as_str().unwrap().to_owned();
            assert_eq!(
                normalize(&state, conversation, "turn", &raw).await.unwrap(),
                item
            );
            let (status, headers, body) = fetch(&state, conversation, &id, "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers["content-type"], "image/png");
            assert_eq!(body, bytes);
            // The phone's thumbnail: a JPEG no larger than asked, named by its
            // own hash, made from the verified original.
            let (status, headers, preview) =
                fetch(&state, conversation, &id, "?maxPixels=1080").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers["content-type"], "image/jpeg");
            assert_eq!(
                headers[crate::media_refs::PREVIEW_SHA256_HEADER],
                hex::encode(Sha256::digest(&preview)).as_str()
            );
            let shown = image::load_from_memory(&preview).unwrap();
            assert_eq!((shown.width(), shown.height()), (1080, 675));
            assert!(preview.len() < bytes.len() / 2);
        }
        // No copy of the original is kept; history keeps one small preview
        // per item (Oct 25 audit: older pictures broke once the file changed).
        let retained = kept_bytes(&state) - before;
        assert!(
            retained > 0 && retained < bytes.len() as u64 / 2,
            "{retained} bytes retained"
        );
        // Changed, then removed: the download says it is gone, never other
        // bytes; the thumbnail is the retained preview, marked as such.
        std::fs::write(&path, screenshot(1600, 999)).unwrap();
        let id = state
            .store
            .list_conversation_files(conversation)
            .await
            .unwrap()[0]
            .id
            .clone();
        for _ in 0..2 {
            let (status, headers, body) = fetch(&state, conversation, &id, "").await;
            assert_eq!(status, StatusCode::GONE);
            assert_eq!(headers["x-wonder-error"], crate::media_refs::GONE_CODE);
            assert_eq!(body, crate::media_refs::GONE.as_bytes());
            for (query, edge) in [
                ("?maxPixels=640", (640, 400)),
                ("?maxPixels=2048", (1080, 675)),
            ] {
                let (status, headers, body) = fetch(&state, conversation, &id, query).await;
                assert_eq!(status, StatusCode::OK, "{query}");
                assert_eq!(headers["content-type"], "image/jpeg");
                assert_eq!(headers[crate::media_refs::ORIGINAL_GONE_HEADER], "1");
                assert_eq!(
                    headers[crate::media_refs::PREVIEW_SHA256_HEADER],
                    hex::encode(Sha256::digest(&body)).as_str()
                );
                assert_ne!(
                    headers[crate::media_refs::PREVIEW_SHA256_HEADER],
                    hex::encode(Sha256::digest(&bytes)).as_str()
                );
                let shown = image::load_from_memory(&body).unwrap();
                assert_eq!((shown.width(), shown.height()), edge, "{query}");
            }
            std::fs::remove_file(&path).unwrap_or_default();
        }
        // Too large a request has no preview to answer with.
        let (status, _, _) = fetch(&state, conversation, &id, "?maxPixels=4000").await;
        assert_eq!(status, StatusCode::GONE);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Convergence review (Oct 26): a path-only imageView recorded before
    /// history kept previews returned its artifact early on refresh and
    /// never gained one, so it still broke once its file changed.
    #[tokio::test]
    async fn path_only_image_views_recorded_earlier_gain_a_retained_preview_on_refresh() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let path = dir.path().join("earlier.png");
        std::fs::write(&path, screenshot(1600, 1000)).unwrap();
        let raw = json!({"id":"earlier-view","type":"imageView","path":path});
        let item = normalize(&state, conversation, "turn", &raw).await.unwrap();
        let id = item["result"]["content"][0]["file"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // As recorded before this change: a reference with no retained preview.
        let media = crate::projects::media_workspace(&state, conversation)
            .await
            .unwrap();
        let derived = std::path::Path::new(&media).join(".wonder/derived");
        for entry in std::fs::read_dir(&derived).unwrap().flatten() {
            if entry.file_name().to_string_lossy().starts_with(&id) {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        assert!(crate::media_refs::retained(std::path::Path::new(&media), &id).is_none());
        // A history refresh names it again, then the file is overwritten.
        let refreshed = normalize(&state, conversation, "turn", &raw).await.unwrap();
        assert_eq!(refreshed, item);
        std::fs::write(&path, screenshot(1600, 999)).unwrap();
        let (status, headers, _) = fetch(&state, conversation, &id, "?maxPixels=640").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[crate::media_refs::ORIGINAL_GONE_HEADER], "1");
        let (status, _, _) = fetch(&state, conversation, &id, "").await;
        assert_eq!(status, StatusCode::GONE);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Claude keeps a tool's screenshot as base64 in its transcript. The
    /// preview refers to that text instead of a decoded copy; a picture no
    /// transcript holds is copied as before.
    #[tokio::test]
    async fn claude_transcript_images_are_referenced_not_copied() {
        let (_fixture, state) = crate::ingestion::tests::fixture().await;
        let conversation = "3a0e4c55-58f8-4e43-9b5a-5c1cbf2f6a10";
        let session = "7f4b1bd1-6d6a-4c8f-a1c4-0c6f3c1f2a11";
        // As Cupola records a Claude Project thread: its Claude session ID.
        crate::test_support::project_thread(
            &state.store,
            conversation,
            "Project",
            state.data_dir.to_str().unwrap(),
            crate::providers::provider_store(&state, wonder_store::AgentFamily::Claude),
            Some("claude:haiku"),
            None,
            "now",
        )
        .await
        .unwrap();
        state
            .store
            .set_project_native_session(conversation, session, "now")
            .await
            .unwrap();
        state
            .store
            .bind_project_runtime(
                conversation,
                wonder_store::AgentFamily::Claude,
                crate::providers::provider_store(&state, wonder_store::AgentFamily::Claude),
                &format!("claude-{session}"),
                Some(session),
                "now",
            )
            .await
            .unwrap();
        let records = tempfile::tempdir().unwrap();
        let transcripts = tempfile::tempdir().unwrap();
        crate::desktop_activity::tests::SESSION_DIRS
            .lock()
            .unwrap()
            .insert(
                session.to_owned(),
                (records.path().to_owned(), transcripts.path().to_owned()),
            );
        let stored = state
            .store
            .project_conversation(conversation)
            .await
            .unwrap()
            .unwrap();
        let transcript =
            crate::desktop_activity::claude_session_transcript(session, &stored.cwd).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let shot = screenshot(1200, 800);
        let other = screenshot(64, 64);
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        std::fs::write(
            &transcript,
            format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{}\"}}}}]}}]}}}}\n",
                encode(&shot)
            ),
        )
        .unwrap();
        let before = kept_bytes(&state);
        let raw = json!({"id":"tool","type":"mcpToolCall","result":{"content":[
            {"type":"text","text":"Took a screenshot"},
            {"type":"image","mimeType":"image/png","data":encode(&shot)},
            {"type":"image","mimeType":"image/png","data":encode(&other)},
        ]}});
        let item = normalize(&state, conversation, "turn", &raw).await.unwrap();
        let referenced = item["result"]["content"][1]["file"].clone();
        let copied = item["result"]["content"][2]["file"].clone();
        assert_eq!(referenced["sha256"], hex::encode(Sha256::digest(&shot)));
        assert_eq!(referenced["width"], 1200);
        let retained = kept_bytes(&state) - before - other.len() as u64;
        assert!(
            retained > 0 && retained < shot.len() as u64 / 2,
            "only the picture no transcript holds is copied; the referenced one keeps a small preview ({retained} bytes)"
        );
        assert_eq!(
            normalize(&state, conversation, "turn", &raw).await.unwrap(),
            item
        );
        for (file, bytes) in [(&referenced, &shot), (&copied, &other)] {
            let (status, _, body) =
                fetch(&state, conversation, file["id"].as_str().unwrap(), "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(&body, bytes);
        }
        // A small picture asked for as a thumbnail comes back unchanged.
        let (status, headers, body) = fetch(
            &state,
            conversation,
            copied["id"].as_str().unwrap(),
            "?maxPixels=320",
        )
        .await;
        assert_eq!((status, &body), (StatusCode::OK, &other));
        assert_eq!(headers["content-type"], "image/png");
        assert!(headers
            .get(crate::media_refs::PREVIEW_SHA256_HEADER)
            .is_none());
        // The transcript was cleaned up: the original is gone, not replaced;
        // the chat's thumbnail comes from the retained preview, marked so.
        std::fs::write(&transcript, "{}\n").unwrap();
        let id = referenced["id"].as_str().unwrap();
        let (status, headers, _) = fetch(&state, conversation, id, "").await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(headers["x-wonder-error"], crate::media_refs::GONE_CODE);
        let (status, headers, body) = fetch(&state, conversation, id, "?maxPixels=320").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[crate::media_refs::ORIGINAL_GONE_HEADER], "1");
        assert_eq!(
            headers[crate::media_refs::PREVIEW_SHA256_HEADER],
            hex::encode(Sha256::digest(&body)).as_str()
        );
        let shown = image::load_from_memory(&body).unwrap();
        assert_eq!((shown.width(), shown.height()), (320, 213));
        crate::desktop_activity::tests::SESSION_DIRS
            .lock()
            .unwrap()
            .remove(session);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Owner (Oct 22): a page an agent published in a T3 Code thread showed
    /// only as a "Tool" row, and the preview's screenshot joined the reply.
    #[tokio::test]
    async fn t3_pages_become_verified_html_files_and_previews_stay_in_details() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let html = format!(
            "<!doctype html><h3>Split view</h3><p>{}</p>",
            "A pane beside the chat. ".repeat(2_000)
        );
        let record = json!({"htmlRender":{"attachmentId":"thread-0f-html","title":"Split: today\nand next",
            "height":1520,"heights":[[375,4184],[728,1506],["wide",3]]},"message":"Shown to the reader above your reply."});
        // As the Claude bridge projects the call, and as Codex reports one.
        let claude = json!({"id":"render","type":"mcpToolCall","server":"Claude","tool":"mcp__t3-code__html_render",
            "status":"completed","arguments":{"title":"Split","height":1520,"html":html},
            "contentItems":[{"type":"inputText","text":record.to_string()}]});
        let codex = json!({"id":"render-codex","type":"mcpToolCall","server":"t3-code","tool":"html_render",
            "status":"completed","arguments":{"title":"Split","html":html},
            "result":{"content":[{"type":"text","text":"Shown"}],"structuredContent":record}});
        for raw in [&claude, &codex] {
            let item = normalize(&state, conversation, "turn", raw).await.unwrap();
            let page = &item["htmlRender"];
            assert_eq!(page["title"], "Split today and next", "{item}");
            let file = &page["file"];
            assert_eq!(file["mimeType"], "text/html");
            // T3's own copy is gone here, so the page is the HTML the agent sent.
            assert_eq!(file["sha256"], hex::encode(Sha256::digest(html.as_bytes())));
            assert_eq!(file["name"], "Split today and next.html");
            assert_eq!(item["arguments"]["html"], "[HTML page, 47 KB]");
            assert!(item.to_string().len() < 4096, "{item}");
            let (status, _, body) =
                fetch(&state, conversation, file["id"].as_str().unwrap(), "").await;
            assert_eq!((status, body), (StatusCode::OK, html.clone().into_bytes()));
            // The phone's row keeps the page and a readable Input.
            let row = crate::history::sanitize_typed_item(&item, None);
            assert_eq!(row["htmlRender"]["file"], *file);
            assert_eq!(row["arguments"]["title"], "Split");
            // A refresh names the same file again.
            let again = normalize(&state, conversation, "turn", raw).await.unwrap();
            assert_eq!(again["htmlRender"], item["htmlRender"]);
        }
        let claude_page = normalize(&state, conversation, "turn", &claude)
            .await
            .unwrap();
        assert_eq!(
            claude_page["htmlRender"]["heights"],
            json!([[375, 4184], [728, 1506]])
        );
        assert_eq!(claude_page["htmlRender"]["height"], 1520);

        // A call still running names no page yet.
        let running = json!({"id":"running","type":"mcpToolCall","server":"Claude",
            "tool":"mcp__t3-code__html_render","status":"inProgress","arguments":{"html":html}});
        let item = normalize(&state, conversation, "turn", &running)
            .await
            .unwrap();
        assert!(item.get("htmlRender").is_none());

        // A preview's screenshot is kept for the call's details only.
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(encoded.into_inner());
        let preview = json!({"id":"preview","type":"mcpToolCall","server":"Claude","tool":"mcp__t3-code__html_preview",
            "status":"completed","arguments":{"html":html,"width":728},
            "contentItems":[{"type":"inputImage","imageUrl":format!("data:image/png;base64,{png}")}]});
        let item = normalize(&state, conversation, "turn", &preview)
            .await
            .unwrap();
        assert_eq!(item["mediaInDetails"], true);
        assert_eq!(item["contentItems"][0]["type"], "wonderArtifact");
        assert_eq!(item["arguments"]["width"], 728);
        assert!(item.get("htmlRender").is_none());

        // Another server's tool of the same name is left alone.
        let other = json!({"id":"other","type":"mcpToolCall","server":"Claude","tool":"mcp__other__html_render",
            "status":"completed","arguments":{"html":"<p>x</p>"}});
        assert_eq!(
            normalize(&state, conversation, "turn", &other)
                .await
                .unwrap(),
            other
        );
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// A visual Cupola's own tool kept reaches the phone like a T3 page, as
    /// each agent family reports the call; a result can name only a visual
    /// of this conversation, and the Input no longer carries the page.
    #[tokio::test]
    async fn show_visual_calls_name_their_kept_visual_for_every_family() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let storage = crate::projects::media_workspace(&state, conversation)
            .await
            .unwrap();
        let html = format!("<h2>Plan</h2><p>{}</p>", "Step. ".repeat(3_000));
        let kept = |seed: &'static str, bytes: Vec<u8>| {
            let (state, storage) = (state.clone(), storage.clone());
            async move {
                store(
                    &state,
                    conversation,
                    &storage,
                    seed,
                    &format!("wonder-visual:m:{seed}"),
                    "Plan.html",
                    "text/html",
                    &bytes,
                )
                .await
                .unwrap()["file"]
                    .clone()
            }
        };
        let first = kept("one", b"<p>Before</p>".to_vec()).await;
        let file = kept("two", html.clone().into_bytes()).await;
        // A picture another tool kept is not a visual.
        let other = store(
            &state,
            conversation,
            &storage,
            "x",
            "tool-media:t:i:0",
            "a.html",
            "text/html",
            b"<p>x</p>",
        )
        .await
        .unwrap()["file"]
            .clone();
        let result =
            json!({"visualId":file["id"],"title":"Plan","kind":"html","replaces":first["id"],
            "shows":{"heading":1,"paragraph":1}})
            .to_string();
        let text = json!([{"type":"inputText","text":result}]);
        // Codex reports a dynamic tool call; Claude's bridge projects one the
        // same way; the ACP bridge reports a call of its `Cupola` server.
        let codex = json!({"id":"v1","type":"dynamicToolCall","tool":"wonder_show_visual","status":"completed",
            "success":true,"arguments":{"title":"Plan","html":html},"contentItems":text});
        let claude = json!({"id":"v2","type":"dynamicToolCall","tool":"wonder_show_visual","status":"completed",
            "success":true,"arguments":{"title":"Plan","html":html},"contentItems":text,"result":{"content":[{"type":"text","text":result}]}});
        let acp = json!({"id":"v3","type":"mcpToolCall","server":"Cupola","tool":"wonder_show_visual","status":"completed",
            "success":true,"arguments":{"title":"Plan","components":{"version":1,"components":[{},{}]}},"contentItems":text});
        for raw in [&codex, &claude, &acp] {
            let item = normalize(&state, conversation, "turn", raw).await.unwrap();
            assert_eq!(item["htmlRender"]["file"], file, "{item}");
            assert_eq!(item["htmlRender"]["title"], "Plan");
            assert_eq!(item["htmlRender"]["replaces"], first["id"]);
            assert!(item.to_string().len() < 4096, "{item}");
            let row = crate::history::sanitize_typed_item(&item, None);
            assert_eq!(row["htmlRender"]["file"], file);
        }
        let item = normalize(&state, conversation, "turn", &codex)
            .await
            .unwrap();
        assert_eq!(item["arguments"]["html"], "[HTML page, 18 KB]");
        let item = normalize(&state, conversation, "turn", &acp).await.unwrap();
        assert_eq!(item["arguments"]["components"], "[2 components]");

        // A result naming another tool's file, or a failed or running call,
        // shows nothing; another server's tool of that name is left alone.
        let mut forged = codex.clone();
        forged["contentItems"][0]["text"] = json!(json!({"visualId":other["id"]}).to_string());
        let mut failed = codex.clone();
        failed["success"] = json!(false);
        let mut running = codex.clone();
        running["status"] = json!("inProgress");
        for raw in [&forged, &failed, &running] {
            let item = normalize(&state, conversation, "turn", raw).await.unwrap();
            assert!(item.get("htmlRender").is_none(), "{item}");
            assert_eq!(item["arguments"]["html"], "[HTML page, 18 KB]");
        }
        // A preview posted nothing; its pictures stay in the call's details.
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(encoded.into_inner());
        let mut preview = codex.clone();
        preview["contentItems"] = json!([
            {"type":"inputText","text":json!({"preview":true,"posted":false}).to_string()},
            {"type":"inputImage","imageUrl":format!("data:image/png;base64,{png}")}]);
        let item = normalize(&state, conversation, "turn", &preview)
            .await
            .unwrap();
        assert!(item.get("htmlRender").is_none(), "{item}");
        assert_eq!(item["mediaInDetails"], true);
        assert_eq!(item["contentItems"][1]["type"], "wonderArtifact");
        assert_eq!(item["arguments"]["html"], "[HTML page, 18 KB]");
        assert!(!item.to_string().contains(&png));
        // The ACP bridge hands the same preview back as an MCP image block.
        let mut acp_preview = acp.clone();
        acp_preview["contentItems"] = json!([
            {"type":"inputText","text":json!({"preview":true,"posted":false}).to_string()},
            {"type":"image","mimeType":"image/png","data":png}]);
        let item = normalize(&state, conversation, "turn", &acp_preview)
            .await
            .unwrap();
        assert!(item.get("htmlRender").is_none(), "{item}");
        assert_eq!(item["mediaInDetails"], true, "{item}");
        assert_eq!(item["contentItems"][1]["type"], "wonderArtifact");
        assert_eq!(item["arguments"]["components"], "[2 components]");
        assert!(!item.to_string().contains(&png));
        let mut foreign = acp.clone();
        foreign["server"] = json!("Claude");
        let item = normalize(&state, conversation, "turn", &foreign)
            .await
            .unwrap();
        assert!(item.get("htmlRender").is_none());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[test]
    fn t3_pages_are_read_only_from_t3_attachments() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("attachments");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("thread-1-html.html"), "<h1>Kept</h1>").unwrap();
        std::fs::write(root.path().join("secret.html"), "<h1>Outside</h1>").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("secret.html"),
            folder.join("link-html.html"),
        )
        .unwrap();
        std::fs::write(folder.join("binary-html.html"), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            t3_page(&folder, "thread-1-html").as_deref(),
            Some(&b"<h1>Kept</h1>"[..])
        );
        for id in [
            "link-html",
            "../secret",
            "..",
            "a/b",
            "thread-1-html.html",
            "",
            "binary-html",
            "missing",
        ] {
            assert!(t3_page(&folder, id).is_none(), "{id}");
        }
        assert_eq!(page_title(Some("  ")), "Page");
        assert_eq!(page_title(Some("a/b:c")), "a b c");
    }

    #[test]
    fn svg_and_media_are_checked_by_their_contents() {
        assert!(valid_svg(
            br#"<?xml version="1.0"?><svg viewBox="0 0 1 1"/>"#
        ));
        assert!(!valid_svg(b"<html></html>"));
        assert!(!valid_svg(b"<!DOCTYPE svg><svg/>"));
        assert!(!valid_svg(&[0xff, 0xfe, b'<']));
        assert!(media_matches(b"ID3\x04", "audio/mpeg"));
        assert!(media_matches(&[0xff, 0xfb, 0x90], "audio/mpeg"));
        assert!(!media_matches(b"RIFF\0\0\0\0AVI ", "audio/wav"));
        assert!(!media_matches(b"\0\0\0\x18ftyp", "audio/ogg"));
    }

    #[test]
    fn rejects_external_resources_and_malformed_images() {
        assert!(
            decode(&json!({"type":"inputImage","imageUrl":"https://example.com/private"})).is_err()
        );
        assert!(decode(&json!({"type":"resource_link","uri":"file:///private/secret"})).is_err());
        assert!(validate(b"\x89PNG\r\n\x1a\n", "image/png").is_err());
        assert!(validate(b"%PDF-1.7", "application/pdf").is_err());
    }
    #[test]
    fn mixed_output_preserves_text_and_locates_only_media() {
        let mut value = json!({"result":{"content":[{"type":"text","text":"Keep this"},{"type":"image","mimeType":"image/png","data":"bad"}]}});
        let mut paths = vec![];
        collect(&value["result"], vec!["result".into()], &mut paths, 0);
        assert_eq!(paths.len(), 1);
        *at_mut(&mut value, &paths[0]).unwrap() = unavailable("Invalid image");
        assert_eq!(value["result"]["content"][0]["text"], "Keep this");
        assert!(!value.to_string().contains("\"data\""));
        let file =
            decode(&json!({"type":"resource","resource":{"text":"notes","mimeType":"text/plain"}}))
                .unwrap();
        assert_eq!(file.0, b"notes");
    }
}
