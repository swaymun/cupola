//! Agent pictures by reference, phone-sized previews, and the preview
//! folder's orphan report.
//!
//! A picture an agent showed already lives on this Mac: the image file a
//! Codex `imageView` named, or base64 text in a Claude transcript. Cupola
//! records where (`conversation_file_sources`) instead of copying it into
//! project-media. Every read is verified against the original's size and
//! SHA-256; a changed or missing source answers "no longer available" and
//! nothing else is ever served in its place. Phones ask for a bounded pixel
//! size and get a JPEG made on demand from the verified original, kept in a
//! small cache bounded by bytes (least recently used goes first).
//!
//! History must not depend on the original alone: each referenced picture
//! also keeps a small retained preview of its own (`<id>.<sha>.preview`
//! beside converted copies). When the original changes or goes, the phone's
//! sized preview comes from it, marked as such; the original download still
//! answers "no longer available" unless the retained bytes are the original.
//!
//! Media publication holds the media write fence; a workspace reset takes it
//! exclusively and retires exactly the folders its chats owned. Large upload
//! staging and private sent-copy I/O run outside the fence. Upload receipts
//! and publication check the reset generation; sent copies refuse a retired
//! path. Bulk writes cannot resurrect a retired folder or hold up its reset.

use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};
use wonder_store::MediaSourceLocation;

const MAX_BYTES: usize = crate::MAX_ATTACHMENT_BYTES;
const fn max_encoded() -> usize {
    MAX_BYTES.div_ceil(3) * 4
}

/// What the phone shows when a referenced picture's original is gone.
pub(crate) const GONE: &str = "This picture is no longer on your computer.";
/// The `x-wonder-error` code for [`GONE`].
pub(crate) const GONE_CODE: &str = "media-source-gone";

/// 410 with [`GONE_CODE`]: the file's bytes are no longer on this Mac.
pub(crate) fn gone_response() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::GONE,
        [(crate::projects::ERROR_CODE_HEADER, GONE_CODE)],
        GONE,
    )
        .into_response()
}
/// The SHA-256 of a phone-sized preview's own bytes. The preview is made
/// only from an original that matched its recorded hash.
pub(crate) const PREVIEW_SHA256_HEADER: &str = "x-wonder-preview-sha256";

/// Says the sized preview came from the retained copy: the original changed
/// or is gone, so a full download answers [`GONE`].
pub(crate) const ORIGINAL_GONE_HEADER: &str = "x-wonder-original-gone";

static MEDIA_WRITES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Held while writing a file into chat media or the preview cache. Hold it
/// once at a time: a second acquisition can wait behind a queued reset.
pub(crate) async fn media_write() -> tokio::sync::RwLockReadGuard<'static, ()> {
    MEDIA_WRITES.read().await
}

/// [`media_write`] from blocking code (a `spawn_blocking` thread or a test).
pub(crate) fn media_write_blocking() -> tokio::sync::RwLockReadGuard<'static, ()> {
    MEDIA_WRITES.blocking_read()
}

pub(crate) fn open_regular(path: &Path) -> Option<(std::fs::File, u64)> {
    // A device node must not be opened at all; the check after opening
    // covers a swap in between.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A named pipe must not hold a request waiting for a writer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    metadata.is_file().then_some((file, len))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// The canonical path of the image file `path` (absolute, or a `file://`
/// URL) when it holds exactly `bytes`.
pub(crate) fn file_holding(path: &str, bytes: &[u8]) -> Option<String> {
    let path = match path.strip_prefix("file://") {
        Some(url) => percent_decode(url)?,
        None => path.to_owned(),
    };
    if !Path::new(&path).is_absolute() {
        return None;
    }
    let canonical = crate::canonical_path::canonicalize(&path).ok()?;
    let (file, len) = open_regular(&canonical)?;
    if len != bytes.len() as u64 {
        return None;
    }
    let mut found = Vec::with_capacity(bytes.len());
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut found)
        .ok()?;
    (found == bytes)
        .then(|| canonical.to_str().map(str::to_owned))
        .flatten()
}

pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The original bytes of a referenced picture, only when they still match
/// the recorded size and SHA-256.
pub(crate) fn read_source(
    location: &MediaSourceLocation,
    byte_size: Option<i64>,
    sha256: Option<&str>,
) -> Option<Vec<u8>> {
    let (Some(byte_size), Some(sha256)) = (byte_size, sha256) else {
        return None;
    };
    let expected = usize::try_from(byte_size)
        .ok()
        .filter(|n| *n <= MAX_BYTES)?;
    let bytes = match location {
        MediaSourceLocation::File { path } => {
            let (file, len) = open_regular(Path::new(path))?;
            if len != expected as u64 {
                return None;
            }
            let mut bytes = Vec::with_capacity(expected);
            file.take(expected as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            bytes
        }
        MediaSourceLocation::Transcript {
            path,
            offset,
            length,
        } => {
            let length = usize::try_from(*length)
                .ok()
                .filter(|n| *n <= max_encoded())?;
            let (mut file, len) = open_regular(Path::new(path))?;
            if offset.checked_add(length as u64)? > len {
                return None;
            }
            file.seek(SeekFrom::Start(*offset)).ok()?;
            let mut encoded = vec![0; length];
            file.read_exact(&mut encoded).ok()?;
            base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .ok()?
        }
    };
    (bytes.len() == expected && hex::encode(digest(&bytes)) == sha256).then_some(bytes)
}

// ---------------------------------------------------------------------------
// Transcript index
// ---------------------------------------------------------------------------

const MIN_ENCODED: usize = 64;

/// The shared transcript reader records image offsets at ingestion. Verify the
/// original bytes before returning a reference, including after a file rewrite.
pub(crate) fn locate_in_transcript(transcript: &Path, bytes: &[u8]) -> Option<(u64, u64)> {
    let wanted = digest(bytes);
    let (offset, length) =
        crate::desktop_activity::transcript::with(transcript, |tail| tail.image(&wanted))??;
    let location = MediaSourceLocation::Transcript {
        path: transcript.to_str()?.to_owned(),
        offset,
        length,
    };
    if read_source(
        &location,
        Some(bytes.len() as i64),
        Some(&hex::encode(wanted)),
    )
    .is_none()
    {
        crate::desktop_activity::transcript::invalidate(transcript);
        return None;
    }
    Some((offset, length))
}

/// Base64 payloads in one JSON record: Claude's `"data":"…"` image sources
/// and `data:…;base64,…` URLs.
pub(crate) fn payloads(
    record: &[u8],
    offset: u64,
    found: &mut HashMap<[u8; 32], (u64, u64)>,
    limit: usize,
) {
    for marker in [&b"\"data\":\""[..], &b";base64,"[..]] {
        for at in memchr::memmem::find_iter(record, marker) {
            if found.len() >= limit {
                return;
            }
            let start = at + marker.len();
            let Some(length) = memchr::memchr(b'"', &record[start..]) else {
                continue;
            };
            if !(MIN_ENCODED..=max_encoded()).contains(&length) {
                continue;
            }
            let Ok(bytes) =
                base64::engine::general_purpose::STANDARD.decode(&record[start..start + length])
            else {
                continue;
            };
            found
                .entry(digest(&bytes))
                .or_insert((offset + start as u64, length as u64));
        }
    }
}

// ---------------------------------------------------------------------------
// Phone-sized previews
// ---------------------------------------------------------------------------

/// The longest edges a phone may ask for. A request is rounded up to one of
/// these so the cache holds few variants; larger requests get the original.
pub(crate) const PREVIEW_EDGES: [u32; 4] = [320, 640, 1080, 2048];
pub(crate) const PREVIEW_CACHE_BYTES: u64 = 200 * 1024 * 1024;
const PREVIEW_QUALITY: u8 = 80;

/// Serialize a variant before the global I/O/decoder bound. The existing
/// verified disk cache then coalesces simultaneous conversions. Every caller
/// still verifies its original so a changed/deleted source cannot be served
/// from a stale cache. Idle keys are weak and removed on the next acquisition.
pub(crate) async fn preview_guard(
    data_dir: &Path,
    sha: &str,
    edge: u32,
) -> (
    tokio::sync::OwnedMutexGuard<()>,
    tokio::sync::OwnedSemaphorePermit,
) {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
        >,
    > = std::sync::LazyLock::new(Default::default);
    let key = format!("{:?}/{sha}/{edge}", data_dir);
    let lock = crate::projects::keyed_lock(&LOCKS, &key).await;
    let permit = PREVIEW_WORK
        .clone()
        .acquire_owned()
        .await
        .expect("preview semaphore stays open");
    (lock, permit)
}

pub(crate) fn preview_edge(requested: u32) -> Option<u32> {
    PREVIEW_EDGES
        .into_iter()
        .find(|edge| requested > 0 && *edge >= requested)
}

/// Raster pictures the host can shrink.
pub(crate) fn resizable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// A JPEG of the picture whose longest edge is at most `edge`, when that is
/// smaller than the original. Decoding is bounded like preview validation;
/// transparent pixels are laid on white. GIFs give their first frame.
pub(crate) fn shrink(bytes: &[u8], edge: u32) -> Option<Vec<u8>> {
    let format = image::guess_format(bytes).ok()?;
    let reader = |bytes| {
        let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
        reader.limits(limits_for_decode());
        reader
    };
    let (width, height) = reader(bytes).into_dimensions().ok()?;
    if width.max(height) <= edge {
        return None;
    }
    let out = jpeg(reader(bytes).decode().ok()?, edge)?;
    (out.len() < bytes.len()).then_some(out)
}

/// `image` as a JPEG no larger than `edge`, transparent pixels on white.
fn jpeg(image: image::DynamicImage, edge: u32) -> Option<Vec<u8>> {
    let image = if image.width().max(image.height()) > edge {
        image.thumbnail(edge, edge)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u32::from(source[3]);
        for channel in 0..3 {
            target[channel] =
                ((u32::from(source[channel]) * alpha + 255 * (255 - alpha)) / 255) as u8;
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, PREVIEW_QUALITY)
        .encode_image(&rgb)
        .ok()?;
    Some(out)
}

fn limits_for_decode() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}

/// A preview and the SHA-256 of its bytes.
pub(crate) struct Preview {
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Default)]
struct CacheIndex {
    loaded: bool,
    tick: u64,
    total: u64,
    /// (original SHA-256, edge) -> (file name, bytes, last use)
    entries: HashMap<(String, u32), (String, u64, u64)>,
}

static CACHES: LazyLock<Mutex<HashMap<PathBuf, CacheIndex>>> = LazyLock::new(Default::default);

/// `<data>/media-cache`, owner-only, never through a symlink.
pub(crate) fn cache_dir(data_dir: &Path) -> Option<PathBuf> {
    let root = crate::canonical_path::canonicalize(data_dir).ok()?;
    let dir = root.join("media-cache");
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let metadata = std::fs::symlink_metadata(&dir).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    let _ = crate::platform::restrict_dir(&dir);
    Some(dir)
}

fn load(dir: &Path, index: &mut CacheIndex) {
    if index.loaded {
        return;
    }
    index.loaded = true;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        match parse_name(&name) {
            Some((original, edge, _)) => {
                let used = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((used, original, edge, name, metadata.len()));
            }
            // Interrupted writes and unknown files are not the cache's.
            None => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    found.sort_by_key(|entry| entry.0);
    for (_, original, edge, name, bytes) in found {
        index.tick += 1;
        index.total += bytes;
        if let Some((old, old_bytes, _)) = index
            .entries
            .insert((original, edge), (name, bytes, index.tick))
        {
            index.total -= old_bytes;
            let _ = std::fs::remove_file(dir.join(old));
        }
    }
}

/// `<original>-<edge>-<preview sha256>.jpg`
fn parse_name(name: &str) -> Option<(String, u32, String)> {
    let stem = name.strip_suffix(".jpg")?;
    let mut parts = stem.split('-');
    let (original, edge, preview) = (parts.next()?, parts.next()?, parts.next()?);
    let hex = |text: &str| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
    (parts.next().is_none() && hex(original) && hex(preview))
        .then(|| Some((original.to_owned(), edge.parse().ok()?, preview.to_owned())))
        .flatten()
}

/// A cached preview, verified against the hash in its name.
pub(crate) fn cached_preview(dir: &Path, original: &str, edge: u32) -> Option<Preview> {
    let mut caches = CACHES.lock().unwrap_or_else(|e| e.into_inner());
    let index = caches.entry(dir.to_owned()).or_default();
    load(dir, index);
    let key = (original.to_owned(), edge);
    let (name, size, _) = index.entries.get(&key)?.clone();
    let path = dir.join(&name);
    let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
    let bytes = open_regular(&path)
        .filter(|_| regular)
        .and_then(|(file, _)| {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()
                .map(|_| bytes)
        });
    let expected = parse_name(&name).map(|(_, _, preview)| preview);
    match bytes {
        Some(bytes) if Some(hex::encode(digest(&bytes))) == expected => {
            index.tick += 1;
            let tick = index.tick;
            if let Some(entry) = index.entries.get_mut(&key) {
                entry.2 = tick;
            }
            if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                let _ = file.set_modified(SystemTime::now());
            }
            Some(Preview {
                sha256: expected?,
                bytes,
            })
        }
        _ => {
            index.entries.remove(&key);
            index.total -= size;
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

/// Keeps a new preview, evicting the least recently used ones past `cap`.
/// A failed write costs only the cache entry.
pub(crate) fn keep_preview(
    dir: &Path,
    original: &str,
    edge: u32,
    preview: &Preview,
    cap: u64,
    generation: u64,
) {
    let size = preview.bytes.len() as u64;
    if size > cap {
        return;
    }
    let name = format!("{original}-{edge}-{}.jpg", preview.sha256);
    let target = dir.join(&name);
    let temporary = dir.join(format!(".{}", uuid::Uuid::new_v4()));
    let _fence = media_write_blocking();
    // A reset since the original was read: the cache folder may be the new
    // one, and this picture belonged to a removed chat.
    if dir
        .parent()
        .is_none_or(|data| self::generation(data) != generation)
    {
        return;
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, &preview.bytes)?;
            std::fs::rename(&temporary, &target)
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return;
    }
    let mut caches = CACHES.lock().unwrap_or_else(|e| e.into_inner());
    let index = caches.entry(dir.to_owned()).or_default();
    load(dir, index);
    index.tick += 1;
    let tick = index.tick;
    match index
        .entries
        .insert((original.to_owned(), edge), (name.clone(), size, tick))
    {
        Some((old, old_size, _)) => {
            index.total = index.total - old_size + size;
            if old != name {
                let _ = std::fs::remove_file(dir.join(old));
            }
        }
        None => index.total += size,
    }
    while index.total > cap {
        let Some(oldest) = index
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.2)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some((old, old_size, _)) = index.entries.remove(&oldest) {
            index.total -= old_size;
            let _ = std::fs::remove_file(dir.join(old));
        }
    }
}

/// At most two previews are made at once; each decodes a whole picture.
pub(crate) static PREVIEW_WORK: LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));

/// The phone-sized preview of a verified original: from the cache, or made
/// now and kept. `None` when the original is already small enough.
/// `generation` is [`generation`] of the data folder from before the
/// original was read.
pub(crate) fn preview(
    dir: Option<&Path>,
    original_sha256: &str,
    edge: u32,
    original: &[u8],
    cap: u64,
    generation: u64,
) -> Option<Preview> {
    if let Some(found) = dir.and_then(|dir| cached_preview(dir, original_sha256, edge)) {
        return Some(found);
    }
    let bytes = shrink(original, edge)?;
    let preview = Preview {
        sha256: hex::encode(digest(&bytes)),
        bytes,
    };
    if let Some(dir) = dir {
        keep_preview(dir, original_sha256, edge, &preview, cap, generation);
    }
    Some(preview)
}

// ---------------------------------------------------------------------------
// Retained previews of referenced pictures
// ---------------------------------------------------------------------------

/// The retained preview's longest edge and its byte bound.
const RETAINED_EDGE: u32 = 1080;
const RETAINED_MAX_BYTES: usize = 1024 * 1024;
/// Originals this small are retained exactly, so they stay downloadable.
const RETAINED_COPY_BYTES: usize = 256 * 1024;

/// `<media>/.wonder/derived`, never through a symlink; made when `create`.
pub(crate) fn derived_dir(media: &Path, create: bool) -> Option<PathBuf> {
    let root = crate::canonical_path::canonicalize(media).ok()?;
    let mut dir = root.clone();
    for component in [".wonder", "derived"] {
        dir.push(component);
        if create {
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
        }
        let metadata = std::fs::symlink_metadata(&dir).ok()?;
        if !metadata.is_dir() {
            return None;
        }
    }
    let _ = crate::platform::restrict_dir(&dir);
    Some(dir)
}

/// What history keeps of a referenced picture: the original when it is
/// small, otherwise a JPEG of at most [`RETAINED_EDGE`] pixels.
fn retained_bytes(original: &[u8]) -> Option<Vec<u8>> {
    if original.len() <= RETAINED_COPY_BYTES {
        return Some(original.to_vec());
    }
    let format = image::guess_format(original).ok()?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(original), format);
    reader.limits(limits_for_decode());
    let bytes = jpeg(reader.decode().ok()?, RETAINED_EDGE)?;
    (bytes.len() <= RETAINED_MAX_BYTES).then_some(bytes)
}

/// Keeps a retained preview of the picture `id` in this chat's media, once.
/// A failure costs only the fallback; the reference itself still works.
pub(crate) fn retain(media: &Path, id: &str, original: &[u8]) {
    if uuid::Uuid::parse_str(id).is_err() || has_retained(media, id) {
        return;
    }
    let Some(bytes) = retained_bytes(original) else {
        return;
    };
    let _fence = media_write_blocking();
    // A reset removed this chat's folder: `create_dir` below cannot rebuild
    // it, because only `.wonder` and `derived` are ever created here.
    let Some(dir) = derived_dir(media, true) else {
        return;
    };
    let name = format!("{id}.{}.preview", hex::encode(digest(&bytes)));
    let temporary = dir.join(format!(".upload-{}", uuid::Uuid::new_v4()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, dir.join(name))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// The retained previews of `id` on disk: (path, the SHA-256 in its name).
fn retained_names(media: &Path, id: &str) -> Vec<(PathBuf, String)> {
    let Some(dir) = derived_dir(media, false) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let prefix = format!("{id}.");
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let sha = name
                .strip_prefix(&prefix)?
                .strip_suffix(".preview")
                .filter(|sha| sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()))?
                .to_owned();
            Some((dir.join(&name), sha))
        })
        .collect()
}

/// Whether `id` has a retained preview on disk (by name; [`retained`] verifies).
pub(crate) fn has_retained(media: &Path, id: &str) -> bool {
    !retained_names(media, id).is_empty()
}

/// The retained preview of `id` and its SHA-256, verified against its name.
pub(crate) fn retained(media: &Path, id: &str) -> Option<Preview> {
    for (path, sha) in retained_names(media, id) {
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
            continue;
        }
        let Some((file, _)) = open_regular(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(RETAINED_MAX_BYTES.max(RETAINED_COPY_BYTES) as u64 + 1)
            .read_to_end(&mut bytes)
            .is_ok()
            && hex::encode(digest(&bytes)) == sha
        {
            return Some(Preview { bytes, sha256: sha });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Workspace reset
// ---------------------------------------------------------------------------

/// Resets per data folder; a preview made from an original read before one
/// is not kept.
static GENERATIONS: LazyLock<Mutex<HashMap<PathBuf, u64>>> = LazyLock::new(Default::default);

pub(crate) fn generation(data_dir: &Path) -> u64 {
    let root =
        crate::canonical_path::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_owned());
    GENERATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&root)
        .copied()
        .unwrap_or_default()
}

/// Folders a reset moved aside, deleted after the fence is released.
const RETIRED_PREFIX: &str = ".retired-media-";

/// Runs `reset` (the store's workspace reset) with chat media writes held
/// off, and once it has committed removes every chat's stored media and the
/// preview cache: exactly the folders listed before the reset committed, so
/// a chat made afterwards keeps its folder and files, including ones its
/// agent writes there without the fence. Runs to the end even if the caller
/// stops waiting. Deleted files are not securely erased.
pub(crate) async fn reset_media<T, E>(
    data_dir: PathBuf,
    reset: impl std::future::Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let task = tokio::spawn(async move {
        let fence = MEDIA_WRITES.write().await;
        let dir = data_dir.clone();
        let old = tokio::task::spawn_blocking(move || old_generation(&dir))
            .await
            .unwrap_or_default();
        let result = reset.await?;
        let dir = data_dir.clone();
        let _ = tokio::task::spawn_blocking(move || retire(&dir, &old)).await;
        drop(fence);
        let _ = tokio::task::spawn_blocking(move || delete_retired(&data_dir)).await;
        Ok(result)
    });
    task.await.expect("workspace reset task does not panic")
}

/// The chat media folders (`project-media/<chat ID>`) and the preview cache
/// as they are now: what a reset about to commit removes. Symlinks and
/// folders that are not Cupola's are never listed.
fn old_generation(data_dir: &Path) -> Vec<PathBuf> {
    let Ok(root) = crate::canonical_path::canonicalize(data_dir) else {
        return Vec::new();
    };
    let real_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let mut targets = Vec::new();
    let media = root.join("project-media");
    if real_dir(&media) {
        if let Ok(entries) = std::fs::read_dir(&media) {
            for entry in entries.flatten() {
                let chat = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok());
                if chat && real_dir(&entry.path()) {
                    targets.push(entry.path());
                }
            }
        }
    }
    let cache = root.join("media-cache");
    if real_dir(&cache) {
        targets.push(cache);
    }
    targets
}

/// Moves the listed folders aside and forgets the preview cache's index.
/// Called with the fence held exclusively, after the reset committed.
fn retire(data_dir: &Path, targets: &[PathBuf]) {
    let root =
        crate::canonical_path::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_owned());
    *GENERATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(root.clone())
        .or_default() += 1;
    CACHES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|dir, _| !dir.starts_with(&root));
    for path in targets {
        // Replaced by a symlink since it was listed: not media; leave it.
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let aside = root.join(format!("{RETIRED_PREFIX}{}", uuid::Uuid::new_v4()));
        if std::fs::rename(path, &aside).is_err() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Deletes folders a reset moved aside, including any a crash left behind.
fn delete_retired(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let retired = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(RETIRED_PREFIX));
        if retired && std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_dir()) {
            // remove_dir_all never follows a symlink inside the folder.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Orphaned preview files
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrphanFolder {
    pub folder: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Default, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrphanReport {
    /// Files no conversation record names, older than the age limit.
    pub orphaned_files: usize,
    pub orphaned_bytes: u64,
    /// Unreferenced files kept because they are newer than the age limit.
    pub recent_files: usize,
    pub recent_bytes: u64,
    /// Files a conversation record still names.
    pub referenced_files: usize,
    pub referenced_bytes: u64,
    /// Orphans per preview folder (a conversation ID), largest first.
    pub folders: Vec<OrphanFolder>,
    pub deleted_files: usize,
    pub deleted_bytes: u64,
    #[serde(skip)]
    pub orphans: Vec<PathBuf>,
}

/// Files under `<data>/project-media/*/.wonder/{attachments,derived,sent}` that
/// no conversation file record names (see [`owner_id`], or as an
/// annotation's source copy), and converters' staging folders, last changed
/// more than `min_age` ago. Symlinks and other folders are never touched.
pub(crate) fn orphan_report(
    data_dir: &Path,
    file_ids: &[String],
    min_age: Duration,
    now: SystemTime,
) -> OrphanReport {
    let known = known_ids(file_ids);
    let mut report = OrphanReport::default();
    let Some(root) = media_root(data_dir) else {
        return report;
    };
    let Ok(folders) = std::fs::read_dir(&root) else {
        return report;
    };
    let mut per_folder: HashMap<String, (usize, u64)> = HashMap::new();
    for folder in folders.flatten() {
        let folder_name = folder.file_name().to_string_lossy().into_owned();
        // Only Cupola's own conversation folders, never through a symlink.
        if uuid::Uuid::parse_str(&folder_name).is_err() {
            continue;
        }
        for sub in ["attachments", "derived", "sent"] {
            let Some(dir) = real_dir(&root, &folder.path().join(".wonder").join(sub)) else {
                continue;
            };
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(metadata) = std::fs::symlink_metadata(file.path()) else {
                    continue;
                };
                let name = file.file_name().to_string_lossy().into_owned();
                let snapshot = sub == "sent" && sent_snapshot(&name);
                let size = if metadata.is_file() {
                    metadata.len()
                } else if metadata.is_dir() && (staging(&name) || snapshot) {
                    folder_bytes(&file.path())
                } else {
                    continue;
                };
                if (metadata.is_file() && owner_id(&name).is_some_and(|id| known.contains(id)))
                    || (snapshot && sent_owner_id(&name).is_some_and(|id| known.contains(id)))
                {
                    report.referenced_files += 1;
                    report.referenced_bytes += size;
                    continue;
                }
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .unwrap_or_default();
                if age < min_age {
                    report.recent_files += 1;
                    report.recent_bytes += size;
                    continue;
                }
                report.orphaned_files += 1;
                report.orphaned_bytes += size;
                let entry = per_folder.entry(folder_name.clone()).or_default();
                entry.0 += 1;
                entry.1 += size;
                report.orphans.push(dir.join(&name));
            }
        }
    }
    report.folders = per_folder
        .into_iter()
        .map(|(folder, (files, bytes))| OrphanFolder {
            folder,
            files,
            bytes,
        })
        .collect();
    report
        .folders
        .sort_by_key(|folder| std::cmp::Reverse(folder.bytes));
    report
}

/// Every name a conversation file record keeps in a preview folder: its ID,
/// and the source copy an annotation of it keeps (`artifact_annotations`).
/// Converted copies are `<id>.<hash>.jpg` and match by their ID part.
fn known_ids(file_ids: &[String]) -> HashSet<String> {
    let mut known: HashSet<String> = file_ids.iter().cloned().collect();
    for id in file_ids {
        known.insert(crate::deterministic_uuid(&format!("artifact-source:{id}")));
    }
    known
}

/// The conversation file a preview-folder name belongs to: the file itself
/// (`<id>`), a converted copy (`<id>.<hash>….jpg`) or a retained preview
/// (`<id>.<hash>.preview`). Anything else, such as a crash's leftover
/// temporary file, belongs to no record and is pruned once old enough.
fn owner_id(name: &str) -> Option<&str> {
    let (id, rest) = name.split_once('.').unwrap_or((name, ""));
    uuid::Uuid::parse_str(id).ok()?;
    let hash = rest.split('.').next().unwrap_or_default();
    let named = rest.is_empty()
        || ((rest.ends_with(".jpg") || rest.ends_with(".preview"))
            && hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    named.then_some(id)
}

/// A converter's staging folder (`agent_images`), left by a crash once old.
fn staging(name: &str) -> bool {
    name.starts_with(".convert-")
}

/// New video snapshots name their source record, keeping every copy while its
/// conversation still owns the file. Legacy random send folders cannot be
/// associated safely with a record; keep them until an explicit workspace reset.
fn sent_owner_id(name: &str) -> Option<&str> {
    let (owner, send) = name.split_once('.')?;
    uuid::Uuid::parse_str(owner).ok()?;
    uuid::Uuid::parse_str(send).ok()?;
    Some(owner)
}
fn sent_snapshot(name: &str) -> bool {
    sent_owner_id(name).is_some()
}

/// Bytes of the regular files directly in `dir` (staging folders are flat).
fn folder_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter_map(|entry| std::fs::symlink_metadata(entry.path()).ok())
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| metadata.len())
            .sum()
    })
}

/// `<data>/project-media`, canonical, when it is a real folder.
fn media_root(data_dir: &Path) -> Option<PathBuf> {
    let root = data_dir.join("project-media");
    let metadata = std::fs::symlink_metadata(&root).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    crate::canonical_path::canonicalize(root).ok()
}

/// `dir` when it and every folder between it and `root` is a real folder.
fn real_dir(root: &Path, dir: &Path) -> Option<PathBuf> {
    let relative = dir.strip_prefix(root).ok()?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current).ok()?;
        if !metadata.is_dir() {
            return None;
        }
    }
    let canonical = crate::canonical_path::canonicalize(&current).ok()?;
    (canonical.starts_with(root) && canonical == current).then_some(current)
}

/// Unreferenced preview files are left alone this long, so a preview being
/// saved right now (file first, record next) is never mistaken for one.
pub(crate) const ORPHAN_MIN_DAYS: u32 = 7;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OrphanParams {
    /// Only files unchanged for at least this many days (default 7, at least 1).
    #[serde(default)]
    pub older_than_days: Option<u32>,
}

async fn orphans(
    state: &crate::AppState,
    params: &OrphanParams,
    apply: bool,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let days = params.older_than_days.unwrap_or(ORPHAN_MIN_DAYS).max(1);
    let ids = match state.store.conversation_file_ids().await {
        Ok(ids) => ids,
        Err(_) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Preview records could not be read. Try again.",
            )
                .into_response()
        }
    };
    let data_dir = state.data_dir.clone();
    let report = tokio::task::spawn_blocking(move || {
        orphan_report(
            &data_dir,
            &ids,
            Duration::from_secs(u64::from(days) * 86_400),
            SystemTime::now(),
        )
    })
    .await;
    let report = match (report, apply) {
        (Ok(mut report), true) => {
            // A preview recorded while the folders were read is kept.
            let Ok(ids) = state.store.conversation_file_ids().await else {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Preview records could not be read. Try again.",
                )
                    .into_response();
            };
            let data_dir = state.data_dir.clone();
            tokio::task::spawn_blocking(move || {
                delete_orphans(&data_dir, &mut report, &ids);
                report
            })
            .await
        }
        (report, _) => report,
    };
    match report {
        Ok(report) => axum::Json(report).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "The preview folder could not be checked.",
        )
            .into_response(),
    }
}

/// `GET /api/v1/host/media/orphans`: what a prune would delete. Changes nothing.
pub(crate) async fn orphan_report_route(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    local: Option<axum::Extension<crate::LocalOwnerAuthority>>,
    axum::extract::Query(params): axum::extract::Query<OrphanParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if local.is_none() {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    orphans(&state, &params, false).await
}

/// `POST /api/v1/host/media/orphans/prune`: deletes what the report lists.
pub(crate) async fn prune_orphans_route(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
    local: Option<axum::Extension<crate::LocalOwnerAuthority>>,
    axum::Json(params): axum::Json<OrphanParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if local.is_none() {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    orphans(&state, &params, true).await
}

/// Deletes the report's orphans that `file_ids` (read again just before)
/// still do not name, then any preview folders left empty. Each file and its
/// folders are checked again first; nothing is reached through a symlink.
pub(crate) fn delete_orphans(data_dir: &Path, report: &mut OrphanReport, file_ids: &[String]) {
    let known = known_ids(file_ids);
    let Some(root) = media_root(data_dir) else {
        report.orphans.clear();
        return;
    };
    for path in std::mem::take(&mut report.orphans) {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        let name = name.to_string_lossy();
        let snapshot = dir.file_name().is_some_and(|part| part == "sent") && sent_snapshot(&name);
        if owner_id(&name).is_some_and(|id| known.contains(id))
            || (snapshot && sent_owner_id(&name).is_some_and(|id| known.contains(id)))
            || real_dir(&root, dir).is_none()
        {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file() && std::fs::remove_file(&path).is_ok() {
            report.deleted_files += 1;
            report.deleted_bytes += metadata.len();
        } else if metadata.is_dir() && (staging(&name) || snapshot) {
            let size = folder_bytes(&path);
            // remove_dir_all never follows a symlink inside the folder.
            if std::fs::remove_dir_all(&path).is_ok() {
                report.deleted_files += 1;
                report.deleted_bytes += size;
            }
        }
        // Folders go only when empty; remove_dir refuses anything else.
        let mut parent = Some(dir);
        while let Some(folder) = parent {
            if folder == root || !folder.starts_with(&root) || std::fs::remove_dir(folder).is_err()
            {
                break;
            }
            parent = folder.parent();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A screenshot-like picture: noisy enough that PNG stays large, with a
    /// transparent left edge.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_fn(width, height, |x, y| {
            let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 7;
            let alpha = if x < 4 { 0 } else { 255 };
            image::Rgba([noise as u8, (noise >> 8) as u8, (x + y) as u8, alpha])
        })
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
        out.into_inner()
    }

    #[test]
    fn transcript_payloads_are_found_once_and_follow_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let first = png(40, 30);
        let second = png(30, 40);
        let encoded = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"user\"}}\n{{\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{}\"}}}}]}}}}\n",
                encoded(&first)
            ),
        )
        .unwrap();
        let (offset, length) = locate_in_transcript(&path, &first).unwrap();
        let location = MediaSourceLocation::Transcript {
            path: path.to_str().unwrap().into(),
            offset,
            length,
        };
        let sha = hex::encode(digest(&first));
        assert_eq!(
            read_source(&location, Some(first.len() as i64), Some(&sha)).as_deref(),
            Some(&first[..])
        );
        // Not there yet, then appended (as a data URL) and found.
        assert!(locate_in_transcript(&path, &second).is_none());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            format!(
                "{{\"image_url\":\"data:image/png;base64,{}\"}}\n",
                encoded(&second)
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(locate_in_transcript(&path, &second).is_some());
        // A different size, hash or a rewritten transcript reads as gone.
        assert!(read_source(&location, Some(first.len() as i64 + 1), Some(&sha)).is_none());
        assert!(read_source(&location, Some(first.len() as i64), Some(&"0".repeat(64))).is_none());
        std::fs::write(&path, "{}\n").unwrap();
        assert!(read_source(&location, Some(first.len() as i64), Some(&sha)).is_none());
        assert!(locate_in_transcript(&path, &first).is_none());
    }

    #[test]
    fn file_sources_must_hold_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot one.png");
        let bytes = png(8, 8);
        std::fs::write(&path, &bytes).unwrap();
        let canonical = crate::canonical_path::canonicalize(&path).unwrap();
        let url = format!("file://{}", canonical.to_str().unwrap().replace(' ', "%20"));
        for spelled in [path.to_str().unwrap(), url.as_str()] {
            assert_eq!(
                file_holding(spelled, &bytes).as_deref(),
                canonical.to_str(),
                "{spelled}"
            );
        }
        assert!(file_holding(path.to_str().unwrap(), &png(9, 9)).is_none());
        assert!(file_holding("relative.png", &bytes).is_none());
        let location = MediaSourceLocation::File {
            path: canonical.to_str().unwrap().into(),
        };
        let sha = hex::encode(digest(&bytes));
        assert!(read_source(&location, Some(bytes.len() as i64), Some(&sha)).is_some());
        std::fs::write(&path, png(8, 9)).unwrap();
        assert!(read_source(&location, Some(bytes.len() as i64), Some(&sha)).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(read_source(&location, Some(bytes.len() as i64), Some(&sha)).is_none());
    }

    #[test]
    fn previews_are_bounded_jpegs_cached_by_bytes() {
        assert_eq!(preview_edge(1), Some(320));
        assert_eq!(preview_edge(1080), Some(1080));
        assert_eq!(preview_edge(1081), Some(2048));
        assert_eq!(preview_edge(4000), None);
        assert_eq!(preview_edge(0), None);
        let original = png(1600, 900);
        let small = shrink(&original, 640).unwrap();
        let decoded = image::load_from_memory(&small).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (640, 360));
        assert_eq!(
            image::guess_format(&small).unwrap(),
            image::ImageFormat::Jpeg
        );
        // Transparent pixels are laid on white, not black.
        assert!(decoded.to_rgb8().get_pixel(0, 0)[2] > 200);
        // Already small enough: the original is served.
        assert!(shrink(&png(300, 200), 320).is_none());

        let data = tempfile::tempdir().unwrap();
        let dir = cache_dir(data.path()).unwrap();
        let sha = |bytes: &[u8]| hex::encode(digest(bytes));
        let made = preview(Some(&dir), &sha(&original), 640, &original, 1 << 20, 0).unwrap();
        assert_eq!(made.sha256, sha(&made.bytes));
        let again = cached_preview(&dir, &sha(&original), 640).unwrap();
        assert_eq!(again.bytes, made.bytes);
        // A damaged cache file is dropped, never served.
        let name = format!("{}-640-{}.jpg", sha(&original), made.sha256);
        std::fs::write(dir.join(&name), b"damaged").unwrap();
        assert!(cached_preview(&dir, &sha(&original), 640).is_none());
        assert!(!dir.join(&name).exists());
        // Least recently used previews go first once past the byte cap.
        let pictures: Vec<Vec<u8>> = (0..3).map(|n| png(900 + n, 700)).collect();
        let size = shrink(&pictures[0], 640).unwrap().len() as u64;
        let cap = size * 2 + size / 2;
        for picture in &pictures[..2] {
            preview(Some(&dir), &sha(picture), 640, picture, cap, 0).unwrap();
        }
        assert!(cached_preview(&dir, &sha(&pictures[0]), 640).is_some());
        preview(Some(&dir), &sha(&pictures[2]), 640, &pictures[2], cap, 0).unwrap();
        assert!(cached_preview(&dir, &sha(&pictures[0]), 640).is_some());
        assert!(cached_preview(&dir, &sha(&pictures[1]), 640).is_none());
        assert!(cached_preview(&dir, &sha(&pictures[2]), 640).is_some());
        let on_disk: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(on_disk <= cap, "{on_disk} > {cap}");
    }

    #[test]
    fn orphan_report_keeps_referenced_recent_and_annotation_files() {
        let data = tempfile::tempdir().unwrap();
        let conv = uuid::Uuid::new_v4().to_string();
        let gone_id = uuid::Uuid::new_v4().to_string();
        let media = data.path().join("project-media");
        let attachments = media.join(&conv).join(".wonder/attachments");
        let derived = media.join(&conv).join(".wonder/derived");
        let gone = media.join(&gone_id).join(".wonder/attachments");
        for dir in [&attachments, &derived, &gone] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let legacy_send = media
            .join(&conv)
            .join(".wonder/sent")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&legacy_send).unwrap();
        std::fs::write(legacy_send.join("legacy.txt"), b"ownership unknown").unwrap();
        let kept = uuid::Uuid::new_v4().to_string();
        let annotation = crate::deterministic_uuid(&format!("artifact-source:{kept}"));
        let orphan = uuid::Uuid::new_v4().to_string();
        let late = uuid::Uuid::new_v4().to_string();
        // A crash's leftovers: a stored copy's old-style temporary file
        // (`<id>.<uuid>`) and a converter's staging folder.
        let leftover = format!("{kept}.{}", uuid::Uuid::new_v4());
        std::fs::write(attachments.join(&leftover), b"tmp-1").unwrap();
        std::fs::create_dir(derived.join(".convert-1")).unwrap();
        std::fs::write(derived.join(".convert-1/image.jpg"), b"jpg").unwrap();
        // A retained preview and a cached conversion belong to their file.
        let retained = format!("{kept}.{}.preview", "b".repeat(64));
        let converted = format!(
            "{kept}.{}.{}-heif-q90-v1.jpg",
            "c".repeat(64),
            "d".repeat(64)
        );
        std::fs::write(derived.join(&retained), b"preview").unwrap();
        std::fs::write(derived.join(&converted), b"jpg").unwrap();
        std::fs::write(attachments.join(&kept), b"kept").unwrap();
        std::fs::write(attachments.join(&annotation), b"note").unwrap();
        std::fs::write(
            derived.join(format!("{kept}.{}.jpg", "a".repeat(64))),
            b"jpg",
        )
        .unwrap();
        std::fs::write(gone.join(&orphan), b"orphan!").unwrap();
        std::fs::write(attachments.join(".upload-1"), b"tmp").unwrap();
        std::fs::write(attachments.join(&late), b"late").unwrap();
        std::os::unix::fs::symlink(gone.join(&orphan), attachments.join("link")).unwrap();
        // Folders reached through a symlink, or not Cupola's, are never read.
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("attachments")).unwrap();
        std::fs::write(outside.path().join("attachments/precious"), b"keep me").unwrap();
        let linked = media.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&linked).unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.join(".wonder")).unwrap();
        std::fs::create_dir_all(media.join("not-a-conversation/.wonder/attachments")).unwrap();
        std::fs::write(media.join("not-a-conversation/.wonder/attachments/x"), b"x").unwrap();
        let ids = vec![kept.clone()];
        let now = SystemTime::now();
        let fresh = orphan_report(data.path(), &ids, Duration::from_secs(3600), now);
        assert_eq!((fresh.orphaned_files, fresh.recent_files), (0, 5));
        assert_eq!(fresh.referenced_files, 5);
        let later = now + Duration::from_secs(7200);
        let mut report = orphan_report(data.path(), &ids, Duration::from_secs(3600), later);
        assert_eq!((report.orphaned_files, report.orphaned_bytes), (5, 22));
        assert!(report.folders.contains(&OrphanFolder {
            folder: gone_id.clone(),
            files: 1,
            bytes: 7
        }));
        // A preview recorded while the folders were read is kept.
        delete_orphans(data.path(), &mut report, &[kept.clone(), late.clone()]);
        assert_eq!(report.deleted_files, 4);
        assert!(!attachments.join(&leftover).exists());
        assert!(!derived.join(".convert-1").exists());
        assert!(derived.join(&retained).exists() && derived.join(&converted).exists());
        assert!(attachments.join(&kept).exists() && attachments.join(&annotation).exists());
        assert!(attachments.join(&late).exists());
        assert!(
            legacy_send.join("legacy.txt").exists(),
            "untracked legacy copies are not guessed to be orphans"
        );
        assert!(std::fs::symlink_metadata(attachments.join("link")).is_ok());
        assert!(outside.path().join("attachments/precious").exists());
        assert!(media
            .join("not-a-conversation/.wonder/attachments/x")
            .exists());
        assert!(!media.join(&gone_id).exists());
        assert!(media.exists());
    }

    /// Oct 25 audit: a workspace reset deleted every chat's records but left
    /// their pictures and files (170 MB) and the preview cache on the Mac.
    #[tokio::test]
    async fn reset_removes_every_chats_media_and_the_preview_cache_once_writers_finish() {
        let data = tempfile::tempdir().unwrap();
        let chat = data
            .path()
            .join("project-media")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(chat.join(".wonder/attachments")).unwrap();
        std::fs::write(chat.join(".wonder/attachments/photo"), b"private").unwrap();
        std::fs::create_dir_all(data.path().join(".retired-media-crashed/x")).unwrap();
        let cache = cache_dir(data.path()).unwrap();
        let original = png(900, 700);
        let sha = hex::encode(digest(&original));
        let generation = generation(data.path());
        let kept = {
            let (cache, sha, original) = (cache.clone(), sha.clone(), original.clone());
            tokio::task::spawn_blocking(move || {
                preview(Some(&cache), &sha, 640, &original, 1 << 20, generation).is_some()
                    && cached_preview(&cache, &sha, 640).is_some()
            })
            .await
            .unwrap()
        };
        assert!(kept);
        std::fs::write(data.path().join("wonder.db"), b"records").unwrap();

        // Oct 26 lead review: a chat made after the reset committed (its
        // agent writes into its folder without the fence) keeps its files.
        let newer = data
            .path()
            .join("project-media")
            .join(uuid::Uuid::new_v4().to_string());
        let made_after_commit = {
            let newer = newer.clone();
            move || {
                std::fs::create_dir_all(&newer).unwrap();
                std::fs::write(newer.join("notes.md"), b"new chat").unwrap();
            }
        };

        // A reset that fails removes nothing.
        let failed = reset_media(data.path().to_owned(), async { Err::<(), _>("failed") }).await;
        assert!(failed.is_err() && chat.exists() && cache.exists());

        // An upload being written finishes before its folder is removed.
        let writing = media_write().await;
        let reset = tokio::spawn(reset_media(data.path().to_owned(), async move {
            made_after_commit();
            Ok::<_, ()>("reset")
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!reset.is_finished() && chat.exists());
        drop(writing);
        assert_eq!(reset.await.unwrap(), Ok("reset"));
        let left: Vec<_> = std::fs::read_dir(data.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let mut left = left;
        left.sort();
        assert_eq!(
            left,
            ["project-media", "wonder.db"],
            "only non-media data and the newer chat stay"
        );
        assert!(!chat.exists());
        assert_eq!(std::fs::read(newer.join("notes.md")).unwrap(), b"new chat");
        // The in-memory cache index went too, and a preview made from an
        // original read before the reset is not kept in the new cache.
        let cache = cache_dir(data.path()).unwrap();
        let after = tokio::task::spawn_blocking(move || {
            let found = cached_preview(&cache, &sha, 640).is_some();
            preview(Some(&cache), &sha, 320, &original, 1 << 20, generation);
            (found, std::fs::read_dir(&cache).unwrap().count())
        })
        .await
        .unwrap();
        assert_eq!(after, (false, 0));
    }

    /// Oct 25 audit: a picture kept by reference broke in history once its
    /// file changed. Its retained preview is small, verified by its own hash,
    /// and is the original only when the original was small enough to copy.
    #[test]
    fn retained_previews_are_small_verified_and_never_pass_for_the_original() {
        let media = tempfile::tempdir().unwrap();
        let (large, small) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let screenshot = png(1600, 1000);
        retain(media.path(), &large, &screenshot);
        let kept = retained(media.path(), &large).unwrap();
        assert_ne!(kept.sha256, hex::encode(digest(&screenshot)));
        assert_eq!(kept.sha256, hex::encode(digest(&kept.bytes)));
        let shown = image::load_from_memory(&kept.bytes).unwrap();
        assert_eq!((shown.width(), shown.height()), (1080, 675));
        assert!(kept.bytes.len() <= RETAINED_MAX_BYTES);
        // Kept once: a second call changes nothing.
        retain(media.path(), &large, &png(1600, 999));
        assert_eq!(retained(media.path(), &large).unwrap().bytes, kept.bytes);
        let icon = png(40, 30);
        retain(media.path(), &small, &icon);
        assert_eq!(retained(media.path(), &small).unwrap().bytes, icon);
        // A damaged retained preview is never served.
        let derived = media.path().join(".wonder/derived");
        std::fs::write(
            derived.join(format!("{large}.{}.preview", kept.sha256)),
            b"x",
        )
        .unwrap();
        assert!(retained(media.path(), &large).is_none());
    }
}
