//! Images in a format the agents read (PNG, JPEG, GIF, WebP) go to them as
//! attached. Any other image (an iPhone's HEIC/HEIF photo, TIFF, BMP…) is
//! converted to JPEG on the host first when its converter supports them
//! (sips on macOS, libheif on Linux). The
//! stored attachment and its integrity hash are never changed: the JPEG is a
//! separate derivative named after its own SHA-256 and verified on reuse.

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use wonder_store::StoredConversationFile;

const DERIVED: [&str; 2] = [".wonder", "derived"];
const SENT: [&str; 2] = [".wonder", "sent"];
const CONVERT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub(crate) const UNREADABLE: &str =
    "This image's format can't be read on this computer. Attach it as a PNG or JPEG.";

/// Formats every agent accepts, recognised by their bytes (attachment files
/// carry no extension and a client's declared type is not trusted).
pub(crate) fn agent_ready(bytes: &[u8]) -> bool {
    agent_mime(bytes).is_some()
}

/// The type of an image every agent accepts, from its bytes.
pub(crate) fn agent_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The HEIF family (an iPhone's HEIC photos and their variants), by the
/// brands of the leading `ftyp` box.
fn heif(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let size = bytes
        .get(..4)
        .map_or(0, |n| u32::from_be_bytes([n[0], n[1], n[2], n[3]]) as usize);
    let end = size.clamp(16, 256).min(bytes.len());
    // The major brand, then the compatible brands after the minor version.
    std::iter::once(8)
        .chain((16..end).step_by(4))
        .filter_map(|at| bytes.get(at..at + 4))
        .any(|brand| {
            matches!(
                brand,
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
            )
        })
}

/// Whether an upload's bytes are the picture type the phone declared.
/// Picture formats Cupola recognizes must match exactly (HEIC and HEIF
/// variants count as one family); an image type it cannot check passes
/// unless the bytes are plainly another recognized format. Other types are
/// not pictures and are stored as declared.
pub(crate) fn content_matches(declared: &str, bytes: &[u8]) -> bool {
    let declared = declared.to_ascii_lowercase();
    if !declared.starts_with("image/") {
        return true;
    }
    let heif_declared = matches!(
        declared.as_str(),
        "image/heic" | "image/heif" | "image/heic-sequence" | "image/heif-sequence"
    );
    if heif(bytes) {
        return heif_declared;
    }
    let declared = if declared == "image/jpg" {
        "image/jpeg"
    } else {
        declared.as_str()
    };
    match image::guess_format(bytes) {
        Ok(format) => format.to_mime_type() == declared,
        Err(_) => {
            !heif_declared
                && !matches!(
                    declared,
                    "image/png"
                        | "image/jpeg"
                        | "image/gif"
                        | "image/webp"
                        | "image/tiff"
                        | "image/bmp"
                )
        }
    }
}

/// The attachments as an agent should receive them: image attachments in
/// another format are replaced, for this turn's input only, by their JPEG
/// derivative. Images and small files carry their verified bytes from
/// `attachments::stored_bytes`. Videos stream into a private, verified file
/// snapshot and carry its path without inline bytes. A picture kept by
/// reference also has no inline bytes. Providers consume these prepared bytes
/// or paths, never reopen the stored original after the integrity check.
pub(crate) async fn for_agent(
    workspace: &str,
    attachments: Vec<StoredConversationFile>,
) -> Result<Vec<(StoredConversationFile, Option<Vec<u8>>)>, String> {
    let mut ready = Vec::with_capacity(attachments.len());
    for file in attachments {
        ready.push(prepare(workspace, file).await?);
    }
    Ok(ready)
}

async fn prepare(
    workspace: &str,
    file: StoredConversationFile,
) -> Result<(StoredConversationFile, Option<Vec<u8>>), String> {
    // A picture kept by reference has no stored copy to send.
    if file.relative_path.is_none() {
        return Ok((file, None));
    }
    if crate::uploads::is_video(file.mime_type.as_deref().unwrap_or_default()) {
        // Agents receive a private verified copy by path, never a gigabyte of
        // inline input or a stored path they could change for the next send.
        let mut record = file;
        record.relative_path = Some(sent_video(workspace, &record).await?);
        return Ok((record, None));
    }
    let bytes = crate::attachments::stored_bytes(workspace, &file).await?;
    let is_image = file
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("image/"));
    if !is_image || agent_ready(&bytes) {
        return Ok((file, Some(bytes)));
    }
    let (file, bytes) = derivative(workspace, &bytes, file).await?;
    Ok((file, Some(bytes)))
}

/// `<workspace>/.wonder/<leaf>`, created owner-only, never through a symlink.
async fn owned_dir(
    workspace: &str,
    components: [&str; 2],
    unavailable: &str,
) -> Result<PathBuf, String> {
    let unavailable = || unavailable.to_owned();
    let root = crate::canonical_path::canonicalize_async(workspace)
        .await
        .map_err(|_| unavailable())?;
    let mut dir = root.clone();
    for component in components {
        dir.push(component);
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(unavailable()),
        }
        let metadata = tokio::fs::symlink_metadata(&dir)
            .await
            .map_err(|_| unavailable())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(unavailable());
        }
    }
    let _ = crate::platform::restrict_dir(&dir);
    Ok(dir)
}

async fn derived_root(workspace: &str) -> Result<PathBuf, String> {
    owned_dir(
        workspace,
        DERIVED,
        "The converted image folder is unavailable.",
    )
    .await
}

/// A copy of a file's verified bytes for an agent that opens attached files
/// itself (Codex and Claude read text, PDFs and other documents with their
/// own tools). Each send writes a new folder with an unguessable name, so the
/// agent opens exactly what was checked: neither a stored file replaced after
/// the check nor a copy planted ahead of time.
pub(crate) async fn sent_copy(
    workspace: &str,
    file: &StoredConversationFile,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let unavailable = "An attached file couldn't be prepared for the agent. Try again.";
    let root = owned_dir(workspace, SENT, unavailable).await?;
    let dir = root.join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir(&dir)
        .await
        .map_err(|_| unavailable.to_owned())?;
    let _ = crate::platform::restrict_dir(&dir);
    // The stored name keeps its extension, which tells the agent's tools how
    // to read it.
    let name = if crate::valid_attachment_name(&file.name) {
        file.name.as_str()
    } else {
        "attachment"
    };
    let path = dir.join(name);
    let written = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o400);
        let mut copy = options.open(&path).await?;
        copy.write_all(bytes).await?;
        copy.sync_all().await
    };
    written.await.map_err(|_| unavailable.to_owned())?;
    Ok(path)
}

/// What a conversion's output depends on besides the source bytes. Change
/// it with the converter's arguments so older copies are not reused.
#[cfg(target_os = "macos")]
const CONVERSION: &str = "sips-q90-v1";
#[cfg(not(target_os = "macos"))]
const CONVERSION: &str = "heif-q90-v1";

/// A converted copy is `<id>.<its sha256>.<source sha256>-<CONVERSION>.jpg`:
/// the same verified source converted the same way is reused.
fn converted_suffix(source_sha256: &str) -> String {
    format!(".{source_sha256}-{CONVERSION}.jpg")
}

/// An earlier conversion of this exact source, verified against its name.
fn reuse(
    dir: &std::path::Path,
    id: &str,
    source_sha256: &str,
) -> Option<(String, Vec<u8>, String)> {
    let prefix = format!("{id}.");
    let suffix = converted_suffix(source_sha256);
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(hash) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(&suffix))
        else {
            continue;
        };
        let Some((file, _)) = crate::media_refs::open_regular(&dir.join(&name)) else {
            continue;
        };
        let mut bytes = Vec::new();
        let read = std::io::Read::read_to_end(
            &mut std::io::Read::take(file, crate::MAX_ATTACHMENT_BYTES as u64 + 1),
            &mut bytes,
        );
        if read.is_ok()
            && agent_ready(&bytes)
            && bytes.len() <= crate::MAX_ATTACHMENT_BYTES
            && hex::encode(Sha256::digest(&bytes)) == hash
        {
            return Some((name.clone(), bytes, hash.to_owned()));
        }
    }
    None
}

/// Converts the verified bytes, never the stored path: the converter reads
/// a private copy, so a file swapped after verification is not what it sees.
async fn derivative(
    workspace: &str,
    original: &[u8],
    file: StoredConversationFile,
) -> Result<(StoredConversationFile, Vec<u8>), String> {
    let dir = derived_root(workspace).await?;
    let id = uuid::Uuid::parse_str(&file.id)
        .map_err(|_| "An attached file is unavailable. Attach it again.".to_owned())?;
    // The record's hash names the verified bytes being converted.
    let source_sha256 = file
        .sha256
        .clone()
        .ok_or("The attachment integrity record is missing.")?;
    let found = {
        let (dir, id, sha) = (dir.clone(), id.to_string(), source_sha256.clone());
        tokio::task::spawn_blocking(move || reuse(&dir, &id, &sha))
            .await
            .ok()
            .flatten()
    };
    if let Some((name, bytes, hash)) = found {
        return Ok((converted_record(file, &name, bytes.len(), hash), bytes));
    }
    let staging = tempfile::Builder::new()
        .prefix(".convert-")
        .tempdir_in(&dir)
        .map_err(|_| "The converted image folder is unavailable.".to_owned())?;
    let source = staging.path().join("original");
    let staged = async {
        let mut copy = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&source)
            .await?;
        copy.write_all(original).await?;
        // Tokio finishes a file write in the background; the converter must
        // not start before every byte is on disk.
        copy.flush().await
    };
    staged
        .await
        .map_err(|_| "The converted image folder is unavailable.".to_owned())?;
    let source = source.as_path();
    let temporary = staging.path().join("image.jpg");
    #[cfg(target_os = "macos")]
    let mut converter = {
        let mut command = tokio::process::Command::new("/usr/bin/sips");
        command
            .args(["-s", "format", "jpeg", "-s", "formatOptions", "90"])
            .arg(source)
            .arg("--out")
            .arg(&temporary);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut converter = {
        // libheif keeps its decoding security limits enabled. Temporary
        // derivatives from multi-image inputs stay inside this owned directory.
        let mut command = tokio::process::Command::new("heif-convert");
        command
            .args(["--quiet", "--codec-threads", "2", "-q", "90"])
            .arg(source)
            .arg(&temporary);
        command
    };
    let converted = tokio::time::timeout(
        CONVERT_TIMEOUT,
        converter
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    let bytes = match converted {
        Ok(Ok(status)) if status.success() => {
            let path = temporary.clone();
            tokio::task::spawn_blocking(move || {
                crate::attachments::read_regular(&path, crate::MAX_ATTACHMENT_BYTES as u64).ok()
            })
            .await
            .ok()
            .flatten()
        }
        _ => None,
    };
    let _ = tokio::fs::remove_file(&temporary).await;
    let bytes = bytes
        .filter(|b| agent_ready(b) && b.len() <= crate::MAX_ATTACHMENT_BYTES)
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let (bytes, hash) = tokio::task::spawn_blocking(move || {
        let hash = hex::encode(Sha256::digest(&bytes));
        (bytes, hash)
    })
    .await
    .map_err(|e| e.to_string())?;
    let name = format!("{id}.{hash}{}", converted_suffix(&source_sha256));
    crate::attachments::write_immutable(&dir.join(&name), &bytes).await?;
    Ok((converted_record(file, &name, bytes.len(), hash), bytes))
}

fn converted_record(
    file: StoredConversationFile,
    name: &str,
    len: usize,
    hash: String,
) -> StoredConversationFile {
    StoredConversationFile {
        mime_type: Some("image/jpeg".into()),
        byte_size: Some(len as i64),
        sha256: Some(hash),
        relative_path: Some(format!("{}/{}/{name}", DERIVED[0], DERIVED[1])),
        ..file
    }
}

/// Stream a verified video to the same private sent-file storage used for
/// ordinary documents. The returned path is relative to the media workspace.
async fn sent_video(workspace: &str, file: &StoredConversationFile) -> Result<String, String> {
    if !crate::valid_attachment_name(&file.name) {
        return Err("The video filename is invalid.".into());
    }
    crate::attachments::check_stored_record(file)?;
    let fence = crate::media_refs::media_write().await;
    let source = crate::locate_attachment(workspace, &file.id, false)
        .await
        .map_err(|_| "The video attachment is unavailable.".to_owned())?;
    let root = owned_dir(workspace, SENT, "The attachment folder is unavailable.").await?;
    let send_id = format!("{}.{}", file.id, uuid::Uuid::new_v4());
    let relative = format!(".wonder/sent/{send_id}/{}", file.name);
    let dir = root.join(send_id);
    let destination = dir.join(&file.name);
    tokio::fs::create_dir(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let _ = crate::platform::restrict_dir(&dir);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut output = options.open(&destination).map_err(|e| e.to_string())?;
    drop(fence);
    let record = file.clone();
    let published = destination.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| {
            crate::uploads::verify(
                &source,
                record.byte_size.unwrap_or_default() as u64,
                record.sha256.as_deref().unwrap_or_default(),
                record.mime_type.as_deref().unwrap_or_default(),
                Some(&mut output),
            )
            .map_err(|(_, text)| text)?;
            output.sync_all().map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&destination);
            let _ = std::fs::remove_dir(&dir);
        }
        result
    })
    .await
    .map_err(|e| e.to_string())??;
    let _fence = crate::media_refs::media_write().await;
    if !published.try_exists().map_err(|e| e.to_string())? {
        return Err("The attachment folder was reset. Attach the file again.".into());
    }
    Ok(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, mime: &str, bytes: &[u8]) -> StoredConversationFile {
        StoredConversationFile {
            id: id.into(),
            conversation_id: "conversation".into(),
            kind: "attachment".into(),
            name: "Photo.heic".into(),
            mime_type: Some(mime.into()),
            byte_size: Some(bytes.len() as i64),
            sha256: Some(hex::encode(Sha256::digest(bytes))),
            relative_path: Some(format!(".wonder/attachments/{id}")),
            state: "available".into(),
            additions: None,
            deletions: None,
            source_id: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    #[cfg(unix)]
    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(16, 16, image::Rgb([200, 40, 40]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    // Owner (Oct 12): a phone screenshot sent as HEIC was rejected twice with
    // "this image format is not supported by Claude".
    // Unix fixtures (absolute /paths, /bin/sh or python3 fakes, sips); Windows paths are not covered yet.
    #[cfg(unix)]
    #[tokio::test]
    async fn heic_photos_reach_the_agent_as_a_verified_jpeg_and_the_original_is_kept() {
        let workspace = tempfile::tempdir().unwrap();
        let attachments = workspace.path().join(".wonder/attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        let (heic_id, png_id) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let source = workspace.path().join("source.png");
        std::fs::write(&source, png()).unwrap();
        #[cfg(target_os = "macos")]
        let made = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "heic"])
            .arg(&source)
            .arg("--out")
            .arg(attachments.join(&heic_id))
            .output()
            .unwrap();
        #[cfg(not(target_os = "macos"))]
        let made = std::process::Command::new("heif-enc")
            .args(["-q", "90", "-o"])
            .arg(attachments.join(&heic_id))
            .arg(&source)
            .output()
            .unwrap();
        assert!(made.status.success(), "fixture HEIC");
        let heic = std::fs::read(attachments.join(&heic_id)).unwrap();
        assert!(!agent_ready(&heic));
        std::fs::write(attachments.join(&png_id), png()).unwrap();
        let originals = vec![
            record(&heic_id, "image/heic", &heic),
            record(&png_id, "image/png", &png()),
        ];

        let root = workspace.path().to_str().unwrap();
        let ready = for_agent(root, originals.clone()).await.unwrap();
        assert_eq!(ready[1].0, originals[1], "a PNG goes as attached");
        let jpeg = &ready[0].0;
        assert_eq!(jpeg.mime_type.as_deref(), Some("image/jpeg"));
        let path = workspace
            .path()
            .join(jpeg.relative_path.as_deref().unwrap());
        assert!(path.starts_with(workspace.path().join(".wonder/derived")));
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(&[0xff, 0xd8, 0xff]));
        assert_eq!(
            jpeg.sha256.as_deref(),
            Some(hex::encode(Sha256::digest(&bytes)).as_str())
        );
        assert_ne!(
            jpeg.sha256, originals[0].sha256,
            "the derivative has its own hash"
        );
        assert_eq!(
            std::fs::read(attachments.join(&heic_id)).unwrap(),
            heic,
            "the original is untouched"
        );
        // Sending again reuses the same verified derivative.
        assert_eq!(
            for_agent(root, originals.clone()).await.unwrap()[0].0,
            *jpeg
        );

        let input = crate::turn_input("Look", root, &ready).await.unwrap();
        assert_eq!(input[1]["url"], data_url("image/jpeg", &bytes));
    }

    fn data_url(mime: &str, bytes: &[u8]) -> String {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("data:{mime};base64,{data}")
    }

    // A12-10 (reviews of 88d328e7 through 12d3abc5): a file swapped after it
    // was verified must not be what Codex or Claude reads. Images and audio go
    // as the verified bytes; text, PDFs and other files as a fresh copy of
    // them, never the stored path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_swapped_after_the_check_is_not_what_the_agent_receives() {
        let workspace = tempfile::tempdir().unwrap();
        let attachments = workspace.path().join(".wonder/attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        let [image, audio, text, pdf] = std::array::from_fn(|_| uuid::Uuid::new_v4().to_string());
        let wav = b"RIFF\x04\0\0\0WAVE".to_vec();
        let notes = b"Ship on Friday.".to_vec();
        let paper = b"%PDF-1.4\n%%EOF\n".to_vec();
        std::fs::write(attachments.join(&image), png()).unwrap();
        std::fs::write(attachments.join(&audio), &wav).unwrap();
        std::fs::write(attachments.join(&text), &notes).unwrap();
        std::fs::write(attachments.join(&pdf), &paper).unwrap();
        let root = workspace.path().to_str().unwrap();
        let mut notes_record = record(&text, "text/plain", &notes);
        notes_record.name = "notes.txt".into();
        let mut paper_record = record(&pdf, "application/pdf", &paper);
        paper_record.name = "paper.pdf".into();
        let ready = for_agent(
            root,
            vec![
                record(&image, "image/png", &png()),
                record(&audio, "audio/wav", &wav),
                notes_record,
                paper_record,
            ],
        )
        .await
        .unwrap();
        for id in [&image, &audio, &text, &pdf] {
            std::fs::write(attachments.join(id), b"swapped by the agent").unwrap();
        }
        let input = crate::turn_input("Look", root, &ready).await.unwrap();
        assert_eq!(input[1]["type"], "image");
        assert_eq!(input[1]["url"], data_url("image/png", &png()));
        assert_eq!(input[2]["type"], "audio");
        assert_eq!(input[2]["url"], data_url("audio/wav", &wav));
        let sent = serde_json::to_string(&input).unwrap();
        assert!(
            !sent.contains(".wonder/attachments"),
            "no stored path for the provider to reopen"
        );
        let opened = |part: &serde_json::Value| {
            let text = part["text"].as_str().unwrap();
            let path = text.rsplit('`').nth(1).unwrap();
            (std::path::PathBuf::from(path), std::fs::read(path).unwrap())
        };
        let (notes_path, notes_read) = opened(&input[3]);
        let (paper_path, paper_read) = opened(&input[4]);
        assert_eq!(notes_read, notes, "the agent opens the verified text");
        assert_eq!(paper_read, paper, "the agent opens the verified PDF");
        assert_eq!(notes_path.file_name().unwrap(), "notes.txt");
        let canonical = crate::canonical_path::canonicalize(workspace.path()).unwrap();
        assert!(notes_path.starts_with(canonical.join(".wonder/sent")));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&notes_path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o400);
        // Each send gets its own copy, so a copy changed after one send is
        // not what the next send hands over.
        let again = crate::turn_input("Look", root, &ready).await.unwrap();
        assert_ne!(opened(&again[3]).0, notes_path);
        assert_ne!(opened(&again[4]).0, paper_path);
    }

    #[tokio::test]
    async fn an_image_the_mac_cannot_read_is_refused_with_a_reason() {
        let workspace = tempfile::tempdir().unwrap();
        let attachments = workspace.path().join(".wonder/attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(attachments.join(&id), b"not an image at all").unwrap();
        let error = for_agent(
            workspace.path().to_str().unwrap(),
            vec![record(&id, "image/heic", b"not an image at all")],
        )
        .await
        .unwrap_err();
        assert_eq!(error, UNREADABLE);
    }
    /// Oct 25 audit: an upload declared as a picture was stored and served as
    /// one whatever its bytes were, failing only when sent.
    #[cfg(unix)]
    #[test]
    fn declared_picture_types_must_match_the_bytes() {
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        let heif = b"\0\0\0\x18ftypmif1\0\0\0\0mif1miaf".to_vec();
        let sequence = b"\0\0\0\x18ftypmsf1\0\0\0\0msf1hevc".to_vec();
        for (declared, bytes, matches) in [
            ("image/png", png(), true),
            ("IMAGE/PNG", png(), true),
            ("image/jpeg", png(), false),
            ("image/png", b"%PDF-1.7 not a picture".to_vec(), false),
            ("image/heic", heic.clone(), true),
            ("image/heif", heic.clone(), true),
            ("image/heif", heif.clone(), true),
            ("image/heic-sequence", sequence.clone(), true),
            ("image/png", heic.clone(), false),
            ("image/heic", png(), false),
            ("image/heic", b"not a photo at all".to_vec(), false),
            // Types Cupola cannot check pass unless plainly another picture.
            (
                "image/svg+xml",
                b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
                true,
            ),
            ("image/x-icon", png(), false),
            // Not pictures: stored as declared.
            ("application/pdf", png(), true),
            ("application/octet-stream", heic, true),
        ] {
            assert_eq!(content_matches(declared, &bytes), matches, "{declared}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_whose_bytes_are_not_the_declared_picture_are_refused() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let conversation = "d19b0f7e-97bb-45d7-8206-324a62386dd9";
        let upload = |mime: &str, bytes: &[u8]| {
            crate::upload_conversation_file(
                axum::extract::State(state.clone()),
                axum::extract::Path(conversation.to_owned()),
                axum::Json(crate::CreateConversationFileRequest {
                    client_upload_id: None,
                    name: "Photo.png".into(),
                    mime_type: Some(mime.into()),
                    content_base64: base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        bytes,
                    ),
                }),
            )
        };
        let refused = upload("image/png", b"%PDF-1.7 not a picture").await;
        assert_eq!(
            refused.status(),
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            upload("image/png", &png()).await.status(),
            axum::http::StatusCode::OK
        );
        let files = state
            .store
            .list_conversation_files(conversation)
            .await
            .unwrap();
        assert_eq!(files.len(), 1, "only the real picture was kept");
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    /// Oct 25 audit: a HEIC photo was converted again on every send, Guide
    /// or retry. A conversion of the same verified source is reused, and
    /// only when its bytes still match the hash in its name.
    #[tokio::test]
    async fn a_conversion_of_the_same_source_is_reused_once_verified() {
        let workspace = tempfile::tempdir().unwrap();
        let attachments = workspace.path().join(".wonder/attachments");
        let derived = workspace.path().join(".wonder/derived");
        std::fs::create_dir_all(&attachments).unwrap();
        std::fs::create_dir_all(&derived).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        // No converter reads these bytes: a fresh conversion would fail.
        let photo = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic not decodable".to_vec();
        std::fs::write(attachments.join(&id), &photo).unwrap();
        let record = record(&id, "image/heic", &photo);
        let jpeg = [&[0xff, 0xd8, 0xff][..], b"converted earlier"].concat();
        let hash = hex::encode(Sha256::digest(&jpeg));
        let source = record.sha256.clone().unwrap();
        let name = format!("{id}.{hash}{}", converted_suffix(&source));
        std::fs::write(derived.join(&name), &jpeg).unwrap();
        let root = workspace.path().to_str().unwrap();
        let ready = for_agent(root, vec![record.clone()]).await.unwrap();
        assert_eq!(ready[0].0.sha256.as_deref(), Some(hash.as_str()));
        assert_eq!(
            ready[0].1.as_deref(),
            Some(&jpeg[..]),
            "the reused bytes are sent"
        );
        assert_eq!(
            ready[0].0.relative_path.as_deref(),
            Some(format!(".wonder/derived/{name}").as_str())
        );
        // A damaged copy, or one of other source bytes, is not reused.
        std::fs::write(derived.join(&name), b"\xff\xd8\xff damaged").unwrap();
        let other = format!("{id}.{hash}{}", converted_suffix(&"0".repeat(64)));
        std::fs::write(derived.join(other), &jpeg).unwrap();
        assert_eq!(for_agent(root, vec![record]).await.unwrap_err(), UNREADABLE);
    }
}
